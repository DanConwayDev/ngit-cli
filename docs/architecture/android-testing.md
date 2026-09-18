# Manual Android smoke tests

Use this runner after changing HTTP/TLS configuration, Reqwest or Rustls
versions/features, Android-specific dependencies, or this test setup. It
builds the current checkout's complete `ngit` and `git-remote-nostr` binaries
for x86_64 Android. It also runs the production TLS module's four loopback
tests inside Android and checks that the ngit CLI starts.

## Experimental support and coverage

Android/Termux support is experimental. This setup grew out of a reported
ngit 3.0 crash in Termux: Reqwest's Android certificate verifier expected a
JVM and application context that a standalone CLI does not have. Android HTTP
clients now use bundled Mozilla roots while retaining certificate and hostname
verification.

The original manual test bundle was validated on an Android 35 x86_64
emulator: both binaries built, ngit started, all four TLS tests passed, and a
public HTTPS request returned HTTP 200. The old verifier panic was also
reproduced. These results establish the TLS fix, not full Termux compatibility;
ARM phones, Android versions beyond that test image, and complete Git, relay,
and signer workflows remain outside the coverage recorded here.

This runner is a manual smoke test, not an Android CI job. Keep it manual
unless CI expansion is explicitly requested. The full Linux suite remains a
separate regression check. Many integration tests require Git and relay
executables; running the full suite on Android would require provisioning
Android-compatible versions of those services and tools. An emulator shell
also differs from Termux. Record each run's platform and scope rather than
assuming a successful smoke test establishes broader support.

## Run

Use an x86_64 Linux host with usable `/dev/kvm`. A VM additionally needs
working nested virtualization. Prefer running on the host if the coding VM's
Nix store is incomplete or its daemon rejects repairs; do not spend the test
session trying to repair privileged infrastructure.

From a checkout visible on that machine:

```sh
nix-shell tools/android/shell.nix
tools/android/android.sh build
tools/android/android.sh start
```

The first setup downloads large Android archives. A quiet `copying path` line
can represent an ongoing download. The Nix expression uses the repository's
`flake.lock` and accepts the Android SDK license via
`android_sdk.accept_license`. It provides Android 35, NDK 27.2.12479018, and
the x86_64 Android Rust target.

Leave the emulator terminal open. In a second terminal, enter the same shell
from the same checkout and run:

```sh
nix-shell tools/android/shell.nix
tools/android/android.sh test
```

Expect four passing TLS tests and a successful CLI help invocation. The tests
cover a trusted test CA, an untrusted issuer, a wrong hostname, and rejection
of an untrusted certificate by the unchanged production builder without a
panic. These runtime checks need no public internet. Initial Nix/Cargo setup
may need downloads. Boot polling has a 180-second deadline, and each TLS
request has a five-second timeout.

To additionally check the production client's bundled Mozilla roots:

```sh
tools/android/android.sh test --online
```

This adds a request to `https://example.com` and should print
`HTTPS response: 200 OK`. It is deliberately optional so the regression tests
do not depend on a public service. No account, signer, or Nostr publication
is involved.

Stop only this emulator when finished:

```sh
tools/android/android.sh stop
```

The runner starts ADB explicitly. If boot polling fails, inspect `adb devices`:
`emulator-5580` should be listed as `device`, not `offline` or `unauthorized`.
Port 5580 must be available; do not run two instances of this runner at once.

## Output and maintenance

All generated files default to the Git-ignored `target/android-smoke/`:
Cargo output, copied binaries, build metadata, and emulator state. Override
`NGIT_ANDROID_TEST_DIR` with an absolute path if necessary, using the same
value in both terminals. The outputs are x86_64 Android executables, not ARM
phone binaries. Never commit these generated files or a copied source tree.

For compilation without downloading the emulator image:

```sh
nix-shell tools/android/shell.nix --arg withEmulator false \
  --run 'tools/android/android.sh build'
```

`tools/android/tls_probe.rs` imports `src/lib/tls.rs` directly, including its
unit tests. Both the example and ngit use the main Cargo manifest and lockfile;
there is no separate probe dependency list to drift during upgrades. The
example requires the opt-in `android-smoke` feature, enabled by the runner.
Default Cargo builds/tests skip it. An `--all-features --all-targets` check
(such as Windows CI) can still compile-check the probe for the host platform;
it never starts an emulator or runs the public HTTPS request. No CI workflow
enters the Android Nix shell. The library's local TLS regression tests remain
part of ordinary CI.

The Android builds use `vendored-openssl` for the Git transport's
cross-compilation requirements. This does not replace the Reqwest/Rustls HTTP verifier.

Keep SDK/image versions consistent between `shell.nix` and `android.sh`.
When changing the runner, validate shell syntax, run
`cargo test --locked --features android-smoke --example android_tls`, and
exercise the Android build/runtime flow where available. Record which target and checks actually
ran; never describe Linux results as Android verification, and state clearly
when an Android run remains pending.
Do not retain an obsolete before-fix client in the permanent runner.

After the emulator is stopped, generated output can be removed when no longer
needed. Follow local cleanup authorization rules; stopping the runner never
deletes build output, SDK packages, or another emulator's state.
