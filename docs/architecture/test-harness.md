# Test Harness Architecture

Design notes for `test_harness` — the integration test crate at
`test_harness/`. Read this if you are writing a new test, debugging a
flaky one, or extending the harness itself.

## TL;DR

`test_harness` drives ngit subcommands against real loopback
infrastructure: vanilla `nostr-relay-builder` relays for non-repo
events, an `ngit-grasp` subprocess for repo events plus git
smart-http, and (where needed) an in-process vanilla git server. Each
test gets its own ports, its own tempdirs, its own keys. Tests are
**parallel by default** — there is no `#[serial]`, no PTY, no
`rexpect`, no exact-stdout assertion. Assertions target git state and
the relay event store; CLI stdout is tertiary.

For the rules every test must follow, see `AGENTS.md` § "Test harness
boundary". This file explains *why* the harness looks the way it
does.

## Architecture

### Process topology

```
test process (tokio runtime)
 │
 ├── grasp subprocess A (in-memory mode, port :0)
 ├── grasp subprocess B (in-memory mode, port :0)
 ├── vanilla relay task(s) (in-process, port :0)
 │
 ├── ngit subprocess (Command, env={NGIT_RELAY_DEFAULT_SET=…, NGITTEST=TRUE, …})
 │     └── git subprocess (inherits env)
 │           └── git-remote-nostr subprocess (inherits env via execve)
 │
 └── git subprocess (Command, env={…})
       └── git-remote-nostr subprocess (inherits env)
```

Three observations underpin this:

1. **`std::process::Command::env()` is per-child, not process-global.**
   Multiple tests can set different `NGIT_RELAY_DEFAULT_SET` values
   concurrently without contamination.
2. **POSIX `execve` propagates parent env to children.** git does not
   scrub env when spawning remote helpers. The chain
   ngit → git → git-remote-nostr inherits every harness env var
   without further plumbing.
3. **GRASP instances are filesystem-backed** (tempdir, bare repos
   under it) but the data path is hidden behind the protocol surface.
   From the test's perspective GRASP is a black-box server-plus-relay
   reachable over `ws://127.0.0.1:port` for nostr and
   `http://127.0.0.1:port` for git smart-http.

### Per-test isolation

| Concern | Mechanism |
|---|---|
| Relay/git ports | OS-assigned via `:0`, held in a `PortReservation` until the fixture's real bind. Retry-on-`AddrInUse` for residual TOCTOU. See `test_harness/src/port.rs`. |
| Relay URL config | `Command::env("NGIT_RELAY_DEFAULT_SET", …)` per spawn — see "Relay-injection mechanism" below |
| `NGITTEST` | Set per spawn for fallback paths; the four `NGIT_*_SET` env vars override the hardcoded localhost defaults |
| Working tree | `tempfile::TempDir` under `std::env::temp_dir()` |
| `GIT_EXEC_PATH` (the dir containing `git-remote-nostr`) | Per-test tempdir; binary copied in once per test |
| User identity | Per-test `nostr::Keys::generate()` |

Nothing is process-global. Nothing requires `#[serial]`.

### Relay-injection mechanism: per-spawn env vars

`src/lib/client.rs::Params::default()` reads, inside the
`NGITTEST=true` branch, four env vars and falls back to hardcoded
`localhost:805x` values when they are unset:

- `NGIT_RELAY_DEFAULT_SET` → `relay_default_set`
- `NGIT_RELAY_BLASTER_SET` → `blaster_relays`
- `NGIT_RELAY_SIGNER_FALLBACK_SET` → `fallback_signer_relays`
- `NGIT_GRASP_DEFAULT_SET` → `grasp_default_set`

Each is parsed as a `;`-separated list of URLs. The harness sets
these on every `Command` it spawns; git inherits via
`std::process::Command`, `git-remote-nostr` via `execve`.

Future renames or splits to the relay-set model are deliberately out
of scope here. Refactor against the harness once the harness is
stable, not the other way around.

### Relay roles: vanilla relays vs GRASP

Two relay primitives:

- **Vanilla nostr relay** (`HarnessBuilder::with_relay`) —
  `nostr-relay-builder::LocalRelay` in-process on `127.0.0.1:0`.
  Accepts any nostr event. Used for kind 0 metadata, kind 10002 relay
  lists, NIP-46 signer events — anything that isn't a repo-specific
  GRASP event.
- **GRASP server** (`HarnessBuilder::with_grasp_server`) — full
  `ngit-grasp` subprocess speaking NIP-01 (only repo-related events:
  kind 30617 announcements, NIP-34 patches, state events) **and**
  git smart-http for the actual git data. Vanilla nostr events like
  kind 0 are rejected.

GRASP cannot stand in for a vanilla relay. Tests that publish user
profiles, relay lists, or NIP-46 signer events need at least one
`with_relay()` instance.

A third primitive,
`HarnessBuilder::with_vanilla_git_server` /
`vanilla_git_server::VanillaGitServer`, provides a non-grasp git
clone URL — needed to exercise the
`is_grasp_server_clone_url == false` branches that GRASP cannot
trigger.

Role labels map onto the env-var schema:

| Role label | Contributes to |
|---|---|
| `with_relay("default")` | `NGIT_RELAY_DEFAULT_SET` |
| `with_relay("blaster")` | `NGIT_RELAY_BLASTER_SET` |
| `with_relay("signer_fallback")` | `NGIT_RELAY_SIGNER_FALLBACK_SET` |
| `with_grasp_server("repo")` | `NGIT_GRASP_DEFAULT_SET` + advertised in repo announcements |
| `with_vanilla_git_server("…")` | role-keyed lookup only — no env injection (ngit has no process-level git-server discovery) |

A test can register multiple instances under the same role; the env
var becomes a `;`-separated list.

### Why subprocess (not library embed) for ngit-grasp

ngit-grasp's own integration tests spawn the binary as a subprocess.
We do the same.

| Subprocess (chosen) | Library embed (deferred) |
|---|---|
| No upstream changes to ngit-grasp | Requires `embed::start()`, `bind :0`, shutdown signal, embedded `Config` builder |
| Higher per-test cost | Cheaper per-test cost |
| Identical to production deployment | Bypasses real wire path |
| Event-store queries go over websocket (real client) | Could query `MemoryDatabase` in-process |

Library embedding remains on the table if subprocess startup becomes
the bottleneck.

### Test API shape

```rust
#[tokio::test]
async fn clone_over_grasp_succeeds() -> Result<()> {
    let harness = Harness::builder()
        .with_relay("default")              // vanilla nostr relay (user index events)
        .with_grasp_server("repo")          // GRASP — git + repo-only relay
        .build().await?;

    let user_keys = harness.generate_user_keys();
    harness.publish_user_metadata(&user_keys).await?;
    let repo_id = harness
        .publish_repo_announcement(&user_keys, &["repo"]).await?;

    let repo = harness.fresh_repo()?;
    let out  = repo.git(["clone", &harness.nostr_url(repo_id), "."]).run()?;

    assert!(out.status.success(), "clone failed: {}", out.stderr_lossy());

    let snapshot = repo.snapshot()?;
    assert!(snapshot.refs.contains_key("refs/heads/main"));

    let events = harness.grasp("repo").events(
        Filter::new().kind(Kind::GitRepoAnnouncement).author(user_keys.public_key())
    ).await?;
    assert_eq!(events.len(), 1);

    Ok(())
}
```

Key shapes:

- **`Harness::builder()`** — fluent async builder; returns when all
  relays and grasp instances are listening.
- **Role labels** — strings, mapped to env-var roles.
- **`harness.fresh_repo()`** — `TempDir`-backed `git init`d repo,
  with git-config pre-populated for the harness's relay roster.
- **`repo.git([…])`** / **`repo.ngit([…])`** — fluent `Command`
  wrappers; harness env baked in.
- **`run()`** returns a struct with `status`, `stdout`, `stderr` and
  convenience accessors. Never panics.
- **`repo.snapshot()`** — refs, HEAD, config keys, working tree
  status. Diffable across before/after.
- **`harness.relay(role).events(filter)`** /
  **`harness.grasp(role).events(filter)`** — real nostr REQ over
  websocket. NIP-01 filter matching.
- **`Drop`** — shuts down all subprocesses and relay tasks.

The API is small on purpose. Scenario builders
(`harness.publish_repo`, `harness.publish_pr`, etc.) accrue in
`test_harness/src/scenarios.rs` as tests demand them.

## Assertion model

Three layers, in priority order.

### 1. Git state (primary)

- `repo.snapshot()` for whole-repo state
- `repo.head()`, `repo.refs()`, `repo.config("key")` for targeted reads

Most tests should pass without inspecting CLI output.

### 2. Nostr event store (secondary)

- `harness.grasp(role).events(filter)` — vec of events matching
  filter
- `events.len()` for count assertions
- Event tag/content inspection for semantics

### 3. CLI output (tertiary, opt-in)

- `out.status.success()` — always check on every `run()`
- `out.stdout_contains(s)` — opt-in, lowercase-normalised substring
- `out.json::<T>()` — for commands that support `--json`

Never assert on exact stdout strings. Never assert on stderr unless
testing an explicit error message contract.

### Init/grasp ordering caveat

When a GRASP server is the target git server, repo announcement
events appear on its relay surface only **after** the `git push` of
the announced repo's git data completes. This is fundamental to
GRASP: the relay is gated on the git-server having the data.

`ngit init`'s subprocess does not return until its internal `git
push` finishes (or fails). The harness uses `Command::output()` /
`wait_with_output()` as the natural barrier. For asynchronous
secondary effects, use `harness.wait_for_event(filter, timeout)`.

### Timing rule: pushes and explicit publishes tick one second

Nostr `created_at` is unix-seconds (NIP-01) — second resolution. Two
events signed by the same key with identical `(kind, tags, content)`
in the same wall-clock second hash to the same event id. For
replaceable / addressable events (kinds 10000–19999 and 30000–39999)
this is nominally fine; in practice `nostr-relay-builder`'s
`MemoryDatabase` adds **superseded** replaceable-event ids to its
internal `deleted_ids` set
(`nostr-database/src/helper.rs::discard_events`), and rejects any
later publish whose id collides with one of those superseded ids
with `"blocked: this event is deleted"` — even though no NIP-09
deletion ever happened. Combined with second-resolution timestamps,
fast back-to-back publishes on the same coordinate flake at roughly
30% on commodity hardware.

The harness sidesteps this by making every operation that publishes
a nostr event end with a one-second sleep, so the next caller's
`Timestamp::now()` lands in a strictly later second:

1. **Push to a nostr remote via `Repo::nostr_push`**, never
   `repo.git(["push", …])`. A push handled by `git-remote-nostr`
   emits an auto-generated kind-30618 state event covering the
   pushed ref(s); `nostr_push` is the variant that ticks afterwards.
2. **`Harness::publish_state_event` and `Harness::publish_repo` tick
   automatically.** No additional sleep needed at the call site.

If you find yourself reaching for `tokio::time::sleep` or a custom
`tick_to_next_second()`, prefer the wrapper that owns the publish.
The bare `test_harness::tick_to_next_second` helper is exported for
the rare case of publishing a custom-built event directly via
`nostr-sdk` (which a test shouldn't, but the escape hatch exists).

The flake this fixes is documented in
`tests/list_state.rs::state_event_takes_precedence_over_advanced_git_server_state`
— preserved as the regression-witness for the rule.

## ngit-grasp dependency

We need the `ngit-grasp` **binary**, not its library. The harness
spawns it as a subprocess; nothing links against `ngit_grasp` as a
crate. Keeps `test_harness`'s cargo dependency tree small.

**Binary discovery** in `Harness::builder()`:

1. `$NGIT_GRASP_BIN` env var, if set.
2. Sibling-clone path
   (`../ngit-grasp/target/release/ngit-grasp`) for the local
   dev pattern.
3. Fail with a clear error.

**Local dev:** `cargo build --release` in `../ngit-grasp` once;
fallback (2) picks it up. Or set `NGIT_GRASP_BIN` in `.envrc`.

**CI:** `ngit-grasp` is wired in via a pinned flake input on the
root `flake.nix`. The dev shell builds it (`doCheck = false`),
exposes the binary on `buildInputs`, and exports `NGIT_GRASP_BIN`
from `shellHook`. CI runs `nix develop --command cargo test`.
Bumping ngit-grasp is a one-line `nix flake update ngit-grasp`.

**Standalone vanilla relay (`with_relay`):** uses
`nostr-relay-builder` in-process. Crates.io 0.44.x.

**Long-term (deferred):** if subprocess startup becomes the
bottleneck, library embedding becomes worth the upstream changes
(`embed::start()`, `bind :0` native, shutdown signal, embedded
`Config` builder). Until then, subprocess is simpler and identical
to production deployment.

## References

- `test_harness/src/clock.rs` — full writeup of the timing rule and
  why one-second ticks are mandatory.
- `test_harness/src/port.rs` — port reservation pattern.
- ngit-grasp's `tests/common/relay.rs` — port allocation and
  subprocess management pattern adopted here.
