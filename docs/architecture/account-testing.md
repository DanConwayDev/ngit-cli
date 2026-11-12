# Account lifecycle validation

Account changes need both isolated CLI coverage and real credential-service
coverage. A file-store redirect cannot distinguish an absent Secret Service
from an unavailable or locked one. The November 2026 audit reproduced exactly
that gap: login fell back successfully, but logout failed on a running D-Bus
session without Secret Service.

The review started from `origin/main` after v3.0.5. The history already had
separate changes for named profiles, preserving a working login on failure,
default-session replacement, deletion and retention semantics, logout-all,
headless logout, named-session fallback and saving without activation. In
particular, `81bf8ff9` covered a missing bus, while `31f5e187` covered fallback
preservation. Neither exercised a running bus without a credential service.
Most integration tests redirected the file store and disabled OS access, so
real keyring lifecycle failures remained invisible. Atomic-write tests also
missed concurrent read/modify/write loss; the added concurrency regression
covers independent CLI processes sharing the same credential files.

## Coverage matrix

All tests use synthetic identities and isolated Git configuration. Production
storage tests clear the child's environment and use a private HOME, XDG paths,
and D-Bus. They never access the developer's credentials or session keyring.

| Area | Scenarios | Coverage |
| --- | --- | --- |
| Backend availability | No session bus; running bus without Secret Service; explicit dead bus; dead default socket; typed activation/access errors | `account_headless_store`, credential-store unit tests |
| Real Secret Service | Automatic OS storage; explicit file storage; mixed inventory; key export; locked deletion refusal; restart/unlock; complete cleanup | Opt-in `production_secret_service_mixed_storage_and_locked_deletion` |
| Profile identity | Existing alias conflicts; inaccessible OS alias remembered by public index; new-profile fallback; same identity with multiple profiles | `account_profile_storage`, `account_create`, real-store test |
| Login selection | Global/local/system precedence; conditional includes; outside-repository refusal; failed login preserving current selection; store without activation | `account_global_config_scope`, `account_scope_isolation`, `account_profile_storage`, `account_create` |
| Remote sessions | Bunker and nostrconnect pairing; rejection; multiple sessions for one identity; default replacement preserving named sessions; stored-session signing; no activation | `nip46_login` |
| Logout | Plain/local/global/profile/npub/all; remote deletion offline; identity-key consent; retention; shared credentials; legacy app keys; broken selectors | `account_logout`, `account_logout_behavior`, `account_logout_all`, production storage tests |
| Retention during outage | Clear all selections without reading locked secrets; retain credential file and public registry; report unavailable accounts | `account_logout_all`, real-store test |
| Inventory | Active account and signer; retained accounts; ambiguous display names; corrupt/missing/unavailable entries; incomplete local override blocking fallback; JSON/human guidance | `account_whoami`, `account_create` |
| Concurrent writers | 24 simultaneous independent imports, index/credential preservation, then concurrent independent deletion | `account_concurrency` |
| Invalid input | Unknown selector; bad secret; encrypted key/password; invalid local scope; unused pairing-relay options | `account_create`, `account_profile_storage`, CLI unit tests |
| Signer transport | Pairing failure; request retry/deadlines; rejection; recovery; one-shot Git push selection | `signer_resend`, `signer_progress`, `git_push_command_signer`, NIP-46 tests |

Tests wait for observable state with bounded deadlines. A successful remote
push follows the harness completion contract; no ordering sleeps are needed.
The real-store test polls the collection's current `Locked` property through
D-Bus calls after restarting the daemon rather than using a cached proxy value.

## Run the automated suites

In the repository development shell:

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- --deny warnings
cargo fmt --all -- --check
```

`account_headless_store` requires `dbus-daemon` on Linux. Its real-keyring test
is explicitly ignored in the normal suite because it also needs
`gnome-keyring-daemon`. Run it as a normal user (not root):

```bash
cargo test --test account_headless_store -- --include-ignored
```

The test starts its own service, verifies that automatic storage really used
the OS backend, and shuts down its private processes. No desktop session or
manual credential-manager prompts are needed. A synthetic password protects
the test's temporary keyring. No production secret is imported.

## Credential-store VM

`test_harness/vm/account-store.nix` builds a disposable NixOS guest with a real
Secret Service and an `audit` user. It mounts the checkout read-only at the
same absolute path, so host-built test executables retain their embedded paths
to `ngit` and `git-remote-nostr`. Build the tests before starting it.

From the checkout, use its absolute path in both places below:

```bash
cargo test --test account_headless_store --no-run
nix build --impure --expr '
  let
    workspace = "/absolute/path/to/ngit";
    f = builtins.getFlake ("git+file://" + workspace);
    pkgs = import f.inputs.nixpkgs { system = builtins.currentSystem; };
  in import (workspace + "/test_harness/vm/account-store.nix") {
    inherit pkgs workspace;
  }
' --out-link target/account-store-vm
target/account-store-vm/bin/run-ngit-account-store-test-vm
```

At the guest's root console, run the executable path printed by Cargo under
its ordinary `audit` user, substituting the actual path/hash:

```bash
su - audit -c '/absolute/path/to/ngit/target/debug/deps/account_headless_store-HASH --include-ignored --nocapture'
poweroff
```

The disk image stays under `target/account-store.qcow2`. The guest contains no
personal credentials, and changes to its keyring cannot affect the host's
keyring. Store guest logs in `/tmp`; the shared checkout is read-only.

## Audit findings and limits

The audit fixed absent-service deletion, silently ignored pairing-relay
options, identity reassignment behind a locked keyring, retention logout that
unnecessarily read locked secrets, unhelpful store-access diagnostics, and lost JSON updates during concurrent
independent account operations.
Separate commits carry the fixes and regressions. An independent manual probe
also paired two `nak bunker` sessions into a real OS keyring, preserved the
agent default with `--no-activate`, stopped the signer, deleted one profile,
verified the other session remained intact, and completed consented cleanup.

This is broad Linux lifecycle coverage, not proof of every possible failure.
Native macOS Keychain and Windows Credential Manager, GUI approval/cancellation,
physical remote-signer apps, competing mutations of the same credential, and crashes
between cross-store operations require separate platform or fault-injection
runs. Multi-backend deletion is not a transaction. The public account index
cannot discover unknown OS entries created outside ngit. These limitations
must not be presented as tested guarantees.
