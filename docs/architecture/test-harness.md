# Test Harness Architecture

Design for ngit's integration test harness — successor to the `test_utils`
crate.

## TL;DR

A new crate, `test_harness`, replaces `test_utils` for all new tests. It uses
**ngit-grasp instances** as both git server and nostr relay, **per-test
dynamic ports**, **per-spawn env vars** to inject relay configuration, and
**`std::process::Command`** (not PTY/rexpect) to invoke binaries. Assertions
target **git state** and **the grasp event store** primarily; CLI stdout is
tertiary. Tests are parallel by default — no `#[serial]`.

Coexistence with the legacy `test_utils` is **hard**: the old harness is
frozen the moment the new one lands, old tests are expendable, and migration
proceeds as a rolling backfill rather than a maintenance burden.

## Why this document exists

`test_utils` reflects design choices made when ngit was a primarily
interactive CLI and there was no specialised git-server-plus-relay
implementation. Three things have since changed:

1. **ngit is now primarily non-interactive.** The `Interactor` defaults to
   non-interactive (`-i` opts in); ~all prompt-bearing commands have flag
   bypasses. Only ~14 interactive-prompt sites remain.
2. **GRASP exists.** ngit-grasp is the reference implementation of a
   git-server-plus-nostr-relay overlay protocol, maintained in this
   organisation, and is the dominant production deployment target. It has
   a working in-memory test mode.
3. **The legacy harness has become a brake on `src/` velocity.** ~140
   `#[serial]` markers, 197 `p.expect()` calls asserting exact CLI strings
   including dialoguer theme renderings, hardcoded relay ports baked into
   event JSON, and 27 `NGITTEST` branches in `src/` shaping production
   behaviour around test needs. Simple `src/` changes routinely break wide
   swathes of tests for cosmetic reasons.

A previous attempt to rewrite the harness (`new-test-harness` branch,
Oct-Nov 2025) stalled. It correctly identified dynamic ports, role-based
relays, and per-spawn env vars as the cheap wins, but never escaped the
PTY/dialoguer assertion model. Five weeks of work migrated 3 test files.
The branch never merged.

This doc is the design for the successor that learns from those mistakes.

## Goals

- **Tests are parallel by default.** No `#[serial]`. Per-test isolation.
- **Tests are non-interactive only.** No PTY, no rexpect, no dialoguer
  rendering. If a flow needs prompts, it isn't tested here.
- **Tests assert on state, not output.** Git state (refs, commits, config,
  working tree) and nostr event store are primary. CLI stdout is tertiary,
  opt-in, and `contains`/JSON-parse based — never exact-string.
- **Tests exercise realistic infrastructure.** A real GRASP server speaks
  the actual git smart-http and NIP-01 relay protocols, not a mock.
- **The harness is small and self-contained.** No imports from `test_utils`.
  Future deletions of `test_utils` should not touch new tests.
- **Migration is incremental, not all-or-nothing.** Each PR ships
  independently. There is never an unmergeable branch.

## Non-goals (for v1)

- Removing `NGITTEST` from `src/`. The env var stays; we extend it
  additively, not replace it.
- Embedding ngit-grasp as a library. We spawn it as a subprocess — same
  pattern as ngit-grasp's own integration tests. Cheaper to land; truer
  test. Library embedding is a future optimisation if startup cost
  becomes a problem.
- Maintaining `test_utils` through `src/` evolution. The old harness is
  frozen on day 1 (see Migration).
- Supporting interactive prompts in tests. The 14 remaining prompt sites
  in `src/` either get flag bypasses (preferred) or lose test coverage.
- Migrating tests by patching them. Tests are rewritten against the new
  harness or deleted.

## Architecture

### Process topology

```
test process (tokio runtime)
 │
 ├─── grasp subprocess A (in-memory mode, port :0 → e.g. 54321)
 ├─── grasp subprocess B (in-memory mode, port :0 → e.g. 54322)
 │      ...
 │
 ├─── ngit subprocess (Command, env={NGIT_RELAY_DEFAULT_SET=ws://127.0.0.1:54321, ...})
 │      ↓ may spawn:
 │      └─── git subprocess (inherits env)
 │             └─── git-remote-nostr subprocess (inherits env via execve)
 │
 └─── git subprocess (Command, env={...})
        └─── git-remote-nostr subprocess (inherits env)
```

Three observations underpin this:

1. **`std::process::Command::env()` is per-child, not process-global.**
   Multiple tests can set different `NGIT_RELAY_DEFAULT_SET` values
   concurrently without contamination.
2. **POSIX `execve` propagates parent env to children.** git does not
   scrub env when spawning remote helpers. This is empirically certified
   by every current `tests/git_remote_nostr/push.rs` test, which depends
   on `NGITTEST=TRUE` reaching `git-remote-nostr` through this chain.
3. **GRASP instances are filesystem-backed** (tempdir, bare repos under
   it) but the data path is hidden behind the protocol surface. From the
   test's perspective GRASP is a black-box server-plus-relay reachable
   over `ws://127.0.0.1:port` for nostr and `http://127.0.0.1:port` for
   git smart-http.

### Per-test isolation

| Concern | Mechanism |
|---|---|
| Relay/git ports | OS-assigned via `:0` bind, held in a `PortReservation` until just before the fixture's real bind. Prevents same-process collisions between parallel `#[tokio::test]`s (the kernel won't reissue a port that's currently bound). A retry-on-`AddrInUse` loop sits behind this as defense-in-depth for the residual microsecond TOCTOU window — never observed to fire locally, kept for CI / loaded hardware. See `test_harness/src/port.rs`. |
| Relay URL config | `Command::env("NGIT_RELAY_DEFAULT_SET", ...)` etc. per spawn |
| `NGITTEST` | Still set per spawn for fallback code paths; new env vars override the hardcoded localhost defaults |
| Working tree | `tempfile::TempDir` under `std::env::temp_dir()` — not `current_dir()` |
| `GIT_EXEC_PATH` (the dir containing `git-remote-nostr`) | Per-test tempdir; binary copied in once per test |
| User identity | Per-test `nostr::Keys::generate()` — no shared test keys |

Nothing is process-global. Nothing requires `#[serial]`.

### Relay-injection mechanism: per-spawn env vars

On `main` today, `src/lib/client.rs::Params` already reads relay
configuration from per-repo git-config keys (`nostr.relay-default-set`,
`nostr.relay-blaster-set`, `nostr.relay-signer-fallback-set`,
`nostr.grasp-default-set`) — but only when `NGITTEST` is unset
(`client.rs:1131`). Under tests, those reads are skipped and the
relay fields fall back to hardcoded localhost ports (8051-8057).

There is **no env-var override path today**. We add one — inside the
existing `NGITTEST=true` branch, so old tests are unaffected:

```rust
// inside Params::default(), in the NGITTEST=true branch:
relay_default_set: env_relay_list("NGIT_RELAY_DEFAULT_SET")
    .unwrap_or_else(|| vec![
        "ws://localhost:8051".to_string(),
        "ws://localhost:8052".to_string(),  // legacy hardcoded fallback
    ]),
// (similarly for blaster_relays / fallback_signer_relays /
//  grasp_default_set, each reading its own env var)
```

The four env vars (matching current `main` field shape):

- `NGIT_RELAY_DEFAULT_SET`
- `NGIT_RELAY_BLASTER_SET`
- `NGIT_RELAY_SIGNER_FALLBACK_SET`
- `NGIT_GRASP_DEFAULT_SET`

Each is parsed as a `;`-separated list of URLs. Empty/unset → legacy
hardcoded behaviour.

This is **purely additive** — old tests, which set `NGITTEST=TRUE` and
no env vars, see no behavioural change. New tests set
`NGITTEST=TRUE` AND the env vars, and the env vars win.

The harness sets these env vars on every `Command` it spawns. Git
inherits them; `git-remote-nostr` inherits them via `execve`. The
chain is identical to how `NGITTEST` reaches the helper today, just
with more keys.

**Relationship to relay-set refactors.** Future renames or splits to
the relay-set model (e.g. splitting `relay_default_set` into separate
user/index roles) are deliberately *out of scope* for the harness
work. The migration order is: build harness against current field
names → migrate tests → then perform any relay-set refactors with a
robust harness in place. Doing those refactors against the legacy
harness has already failed once.

### Why subprocess (not library embed) for ngit-grasp

ngit-grasp's own integration tests spawn the compiled `ngit-grasp`
binary as a subprocess in `tests/common/relay.rs`. We do the same.

| Subprocess (chosen) | Library embed (deferred) |
|---|---|
| No upstream changes to ngit-grasp | Requires `embed::start()`, `bind :0`, shutdown signal, embedded `Config` builder |
| Higher per-test cost (process startup, `git init --bare`) | Cheaper per-test cost |
| Identical to production deployment | Bypasses real wire path |
| Event-store queries go over websocket (real client) | Could query `MemoryDatabase` in-process |
| ngit-grasp can evolve independently | Tighter coupling |

The user accepted per-test cost as the right trade. Library embedding
remains on the table if measurements show subprocess startup is the
bottleneck.

### Relay roles: vanilla relays vs GRASP

The harness offers two relay primitives:

- **Vanilla nostr relay** (`with_relay`) — accepts arbitrary nostr
  events. Used for user metadata (kind 0), relay lists (kind 10002),
  signer connect events, and anything else not specific to a git
  repository. Implementation: `nostr-relay-builder` running in-process
  in the test runtime, bound to `127.0.0.1:0`.
- **GRASP server** (`with_grasp_server`) — a full `ngit-grasp`
  subprocess speaking both NIP-01 (for repo-related events only —
  kind 30617 repo announcements, NIP-34 patches, state events, etc.)
  and git smart-http (for the actual git data). Vanilla nostr events
  like kind 0 or 10002 are rejected.

GRASP cannot stand in for a vanilla relay. Tests that publish user
profiles, relay lists, or NIP-46 signer events need at least one
`with_relay()` instance.

Role labels map onto the env-var schema:

| Role label | Contributes to |
|---|---|
| `with_relay("default")` | `NGIT_RELAY_DEFAULT_SET` |
| `with_relay("blaster")` | `NGIT_RELAY_BLASTER_SET` |
| `with_relay("signer_fallback")` | `NGIT_RELAY_SIGNER_FALLBACK_SET` |
| `with_grasp_server("repo")` | `NGIT_GRASP_DEFAULT_SET` + used as git-server URL in the published repo announcement |

A test can register multiple instances under the same role; the env
var becomes a `;`-separated list of URLs.

### Test API shape (sketch)

```rust
#[tokio::test]
async fn clone_over_grasp_succeeds() -> Result<()> {
    let harness = Harness::builder()
        .with_relay("default")              // vanilla nostr relay (user index events)
        .with_grasp_server("repo")          // GRASP — git + repo-only relay
        .build().await?;

    let user_keys = harness.generate_user_keys();
    harness.publish_user_metadata(&user_keys).await?;            // kind 0, kind 10002 → "default" relay
    let repo_id = harness
        .publish_repo_announcement(&user_keys, &["repo"]).await?; // kind 30617 → "repo" grasp

    let repo = harness.fresh_repo()?;       // TempDir, env-configured
    let out  = repo.git(["clone", &harness.nostr_url(repo_id), "."]).run()?;

    assert!(out.status.success(), "clone failed: {}", out.stderr_lossy());

    let snapshot = repo.snapshot()?;
    assert!(snapshot.refs.contains_key("refs/heads/main"));

    let events = harness.grasp("repo").events(
        Filter::new().kind(Kind::GitRepoAnnouncement).author(user_keys.public_key())
    ).await?;
    assert_eq!(events.len(), 1);

    // harness shuts down all relays / grasp instances on drop
    Ok(())
}
```

Key shapes:

- **`Harness::builder()`** — fluent builder, async. Returns when all
  relays and grasp instances are listening.
- **Role labels** — strings (`"default"`, `"blaster"`, `"repo"`, ...),
  not positional indices. Maps to env-var roles inside `client.rs`.
- **`harness.fresh_repo()`** — `TempDir`-backed `git init`d repo, with
  git-config pre-populated to match the harness's relay roster, and
  with the harness env vars baked into the `Repo` for all spawned
  commands.
- **`repo.git([...])`** / **`repo.ngit([...])`** — fluent `Command`
  wrappers; inherit env from the harness automatically.
- **`run()`** returns a struct with `status`, `stdout: Vec<u8>`,
  `stderr: Vec<u8>`, plus convenience accessors. Never panics; tests
  decide what's an assertion.
- **`repo.snapshot()`** — returns a serializable struct: refs (name →
  oid), HEAD, config keys of interest, working tree status. Diffable
  across before/after.
- **`harness.relay(role).events(filter)`** /
  **`harness.grasp(role).events(filter)`** — queries the relay's
  event store via a real nostr REQ over websocket. NIP-01 filter
  matching, no mock shortcuts.
- **`Drop`** — shuts down all subprocesses and relay tasks. No
  `shutdown()` closure to remember.

The API is small on purpose. Scenario builders (e.g. publishing a
proposal, populating a remote repo with commits) accrue as migration
demands them — but always as helpers in `test_harness`, never as
imports from `test_utils`.

## Assertion model

Three layers, in priority order:

### 1. Git state (primary)

- `repo.snapshot()` for whole-repo state
- `repo.head()`, `repo.refs()`, `repo.config("key")` for targeted reads
- `assert_eq!(before, after)` or `assert_diff!(before, after, expect: ...)`
  for structured diffs

Most tests should pass without ever inspecting CLI output.

### 2. Nostr event store (secondary)

- `harness.grasp(role).events(filter)` — vec of events matching filter
- `events.len()` for count assertions
- Event tag / content inspection for semantics

This replaces the `relay.events: Vec<Event>` field on the legacy mock
relay and the `get_events_from_cache()` LMDB reader.

### 3. CLI output (tertiary, opt-in)

- `out.status.success()` — always check this on every `run()`
- `out.stdout_contains(s)` — opt-in substring assertion, lowercase-normalised
- `out.json::<T>()` — parse stdout as JSON for commands that support
  `--json` (`ngit pr list --json`, `ngit issue list --json`,
  `ngit repo --json`, `ngit account whoami --json`)

Never assert on exact stdout strings. Never assert on stderr unless
testing an explicit error message contract.

### Init/grasp ordering caveat

When a GRASP server is the target git server, repo announcement events
appear on its relay surface only **after** the `git push` of the
announced repo's git data completes. This is fundamental to GRASP:
the relay is gated on the git-server having the data.

`ngit init`'s subprocess does not return until its internal `git push`
finishes (or fails). The test harness therefore uses `Command::output()`
/ `wait_with_output()` as the natural barrier — when the subprocess
exits, the grasp instance has either already received the announcement
or won't.

Tests that await asynchronous secondary effects can use a thin
`harness.wait_for_event(filter, timeout)` helper, but for the common
case the subprocess exit barrier is sufficient.

### Timing rule: pushes and explicit publishes tick one second

Nostr `created_at` is unix-seconds (NIP-01) — second resolution. Two
events signed by the same key with identical `(kind, tags, content)`
in the same wall-clock second hash to the same event id. For
replaceable / addressable events (kinds 10000–19999 and 30000–39999)
that's nominally fine; in practice `nostr-relay-builder`'s
`MemoryDatabase` adds **superseded** replaceable-event ids to its
internal `deleted_ids` set (see
`nostr-database/src/helper.rs::discard_events`), and rejects any later
publish whose id collides with one of those superseded ids with the
wire message `"blocked: this event is deleted"` — even though no
NIP-09 deletion ever happened. Combined with second-resolution
timestamps, fast back-to-back publishes on the same coordinate flake
at roughly 30% on commodity hardware.

The harness sidesteps this by making every operation that publishes a
nostr event end with a one-second sleep, so the next caller's
`Timestamp::now()` lands in a strictly later second and id collisions
are impossible by construction. The rule has two prongs you'll touch
when writing or migrating a test:

1. **Always push to a nostr remote via `Repo::nostr_push`**, never
   `repo.git(["push", ...])`. A push handled by `git-remote-nostr`
   emits an auto-generated kind-30618 state event covering the
   pushed ref(s); `nostr_push` is the variant that ticks afterwards.
   Tests that bypass it will pass in isolation and flake under
   parallel load.

2. **`Harness::publish_state_event` and `Harness::publish_repo` tick
   automatically.** No additional sleep needed at the call site.

If you find yourself reaching for `tokio::time::sleep` or a custom
`tick_to_next_second()` call in a test, prefer the wrapper that owns
the publish — the rule belongs in the harness, not at the call site.
The bare [`test_harness::tick_to_next_second`] helper is exported
public for the rare case where a test publishes a custom-built event
directly via `nostr-sdk` (which it shouldn't, but the escape hatch
exists).

The flake this fixes is documented in
`tests/list_state.rs::state_event_takes_precedence_over_advanced_git_server_state`
— preserved as the regression-witness for the rule.

## Required `src/` changes

### PR 1: env-var reads inside the existing `NGITTEST=true` branch

In `src/lib/client.rs::Params::default()`, the `NGITTEST=true` branch
currently returns a hardcoded `vec!["ws://localhost:8051", ...]` for
each relay field. Wrap each of those with a read of the corresponding
env var, falling back to the hardcoded vec when unset:

- `NGIT_RELAY_DEFAULT_SET` → overrides `relay_default_set`
- `NGIT_RELAY_BLASTER_SET` → overrides `blaster_relays`
- `NGIT_RELAY_SIGNER_FALLBACK_SET` → overrides `fallback_signer_relays`
- `NGIT_GRASP_DEFAULT_SET` → overrides `grasp_default_set`

Each is parsed as a `;`-separated list of URLs. Empty/unset → legacy
hardcoded behaviour preserved.

~30 lines, no API change, no breaking change to old tests. The
mechanism reuses the env-propagation chain already certified by
`NGITTEST` itself.

### Future (not blocking)

- Remove the `NGITTEST` gate around git-config relay reading in
  `Params::new()` (~7 lines). Lets git-config-based injection also
  work in tests. Strictly an improvement; not required for v1.
- Replace remaining `NGITTEST` branches in `src/` (spinners, cache
  path, fallbacks for other settings) with proper config injection.
  Larger refactor; tracked separately.
- Relay-set model refactors (renames, splits — e.g. the unmerged
  `pr/rename-split-relay-sets` work). Deferred until after migration
  so the new harness can absorb the refactor cleanly.
- Embed ngit-grasp as a library. Requires upstream additions:
  `embed::start()`, `bind :0` native, shutdown signal, embedded
  `Config` builder. Triggered if subprocess cost becomes the
  bottleneck.

## ngit-grasp dependency

We need the `ngit-grasp` **binary**, not its library. The test
harness spawns it as a subprocess; nothing links against
`ngit_grasp` as a crate. This deliberately keeps
`test_harness`'s cargo dependency tree small (ngit-grasp's library
unconditionally pulls in `clap`, `dotenvy`, `tracing-subscriber`,
etc. — undesirable in a test crate).

**Binary discovery:** `Harness::builder()` resolves the path in this
order:

1. `$NGIT_GRASP_BIN` env var, if set.
2. A conventional sibling-clone path (`../ngit-grasp/target/release/ngit-grasp`)
   — convenient for the local dev pattern of having both repos
   checked out side by side.
3. Fail with a clear error pointing at the setup docs.

**Local dev:** `cargo build --release` in `../ngit-grasp` once;
fallback (2) picks it up. Or set `NGIT_GRASP_BIN` in a `.envrc` /
shell config.

**CI:** `ngit-grasp` is wired in via a pinned flake input on the root
`flake.nix`. The dev shell builds it (with `doCheck = false` to skip
ngit-grasp's own unit tests, which expect git in PATH and don't run in
the Nix build sandbox), exposes the binary on `buildInputs`, and
exports `NGIT_GRASP_BIN` from `shellHook`. CI already runs everything
through `nix develop --command cargo test`, so the test harness
automatically picks up the pinned binary with no extra workflow steps.
Bumping ngit-grasp is a one-line `nix flake update ngit-grasp` plus
the resulting `flake.lock` change.

Why a flake input rather than `cargo install --git` in the GitHub
workflow:

- The project's CI is already nix-based; a flake input is cheaper
  than introducing a parallel cargo-cache code path.
- A locked flake rev gives bit-for-bit reproducible builds without
  hand-rolled cache-invalidation keys.
- Local dev and CI use the *same* mechanism — no drift between the
  two binary paths.

The sibling-clone fallback in `test_harness/src/grasp.rs` stays for
local-dev convenience when working without the nix shell.

**Standalone vanilla relay (`with_relay`):** uses `nostr-relay-builder`
in-process. Crates.io 0.44.x is sufficient for v1 (accept-all-events
behaviour). If newer relay-builder features are needed, revisit —
options include git-pinning to match the rev ngit-grasp uses, or
spawning a second `ngit-grasp` if it grows a vanilla-relay mode.

**Version alignment:** ngit-grasp's pinned `nostr-sdk` rev should be
compatible with ngit's `nostr-sdk = 0.44.1` (crates.io). Both
projects are MIT-licensed.

**Long-term (deferred):** if subprocess startup becomes the
test-suite bottleneck, library embedding becomes worth the four small
upstream changes identified in earlier audits (`embed::start()`,
`bind :0` native, shutdown signal, embedded `Config` builder). Until
then, subprocess is simpler and identical to production deployment.

## Migration plan

### Hard coexistence rules (commit once, enforce always)

1. **`test_utils` is frozen on day 1.** No fixes, no adaptations, no
   patches. Bugs are not fixed; they cause `#[ignore]` or deletion.
2. **Old tests are expendable.** When `src/` changes break them:
   - Preferred: migrate the affected tests in the same PR.
   - Acceptable: `#[ignore]` with a `// FIXME(harness-migration): ...`
     comment and a tracking note.
   - Acceptable: delete entirely if coverage already exists in the new
     harness.
   - **Forbidden: patch the old harness or old test to keep it green.**
3. **No `test_utils` imports in `test_harness` or in new tests.** The
   boundary is hermetic. Helpers are rebuilt fresh if needed.
4. **No half-migrated test files.** A file is fully old (legacy
   `test_utils`) or fully new (`test_harness`). The intermediate state
   is forbidden.
5. **No `#[serial]` in new tests.** Ever.
6. **No PTY/rexpect/dialoguer assertions in new tests.** Ever.

### Test directory layout

Cargo doesn't auto-discover nested integration tests (`tests/foo/*.rs`
aren't picked up by default). To get a clean `tests/legacy/` boundary
we use explicit `[[test]]` entries in `Cargo.toml`:

```toml
[[test]]
name = "legacy_ngit_init"
path = "tests/legacy/ngit_init.rs"

[[test]]
name = "legacy_ngit_send"
path = "tests/legacy/ngit_send.rs"
# ...one entry per old test file
```

PR 1 moves every existing `tests/*.rs` file into `tests/legacy/`
(with `[[test]]` entries added), so the boundary is visible from
day one. New tests live at `tests/*.rs` directly.

As tests are migrated, the corresponding `[[test]]` entry is deleted
from `Cargo.toml` and the file deleted from `tests/legacy/`. When the
last entry is gone, the directory disappears with it.

### PR sequence

**PR 1 — Foundation:**
- New `test_harness` crate skeleton (port allocator, grasp instance
  manager, vanilla-relay manager, `Harness` builder, `Repo` fixture,
  snapshot helper)
- Env-var reads inside the `NGITTEST=true` branch in
  `src/lib/client.rs::Params::default()` (~30 LoC, additive)
- Move existing `tests/*.rs` → `tests/legacy/*.rs`; add `[[test]]`
  entries to `Cargo.toml`
- One lighthouse test: clone-over-grasp at `tests/clone.rs`
- Old tests UNTOUCHED in content (just relocated)
- CI runs both legacy and new
- Mergeable in isolation, small, additive, low risk

**PR 2 — Freeze declaration:**
- Add `#![doc = "DEPRECATED — frozen, do not modify. New tests use \
  `test_harness`."]` to `test_utils/src/lib.rs`
- Update `AGENTS.md` to mandate the rules above
- Mergeable in isolation

**PR 3-N — Rolling migration:**
- Each PR migrates one logical area (init, push, send, pr_checkout,
  fetch). Rough ordering:
  1. `tests/legacy/git_remote_nostr/main.rs` — clone flow
  2. `tests/legacy/ngit_init.rs` — init + grasp announcement
  3. `tests/legacy/git_remote_nostr/push.rs` — push rstest groups
  4. `tests/legacy/ngit_send.rs` — PR send flow
  5. `tests/legacy/ngit_pr_checkout.rs` — already non-PTY, retarget
     setup onto scenario builders
  6. `tests/legacy/git_remote_nostr/fetch.rs`
  7. `tests/legacy/ngit_login.rs` — interactive parts dropped,
     non-interactive paths covered
  8. `tests/legacy/git_remote_nostr/list.rs`
- Each PR adds scenario builders to `test_harness` as that area
  demands them.
- Each PR deletes the migrated old file and its `[[test]]` entry in
  the same commit.

**PR Final — Bury the body:**
- Delete `test_utils` crate
- Remove `tests/legacy/` directory
- Optionally remove the additive env-var branch (or keep — harmless)

### Scope of v1's "complete" state

Migration ends when:
- `test_utils` deletion no longer breaks any test
- `tests/` contains only `test_harness`-based tests
- The `#[ignore]` backlog from migration is zero

This is bounded by the test count (~134 tests, of which many are
rstest variations of a smaller scenario set — closer to ~20 distinct
scenarios). With focused work, weeks not months.

## Anti-patterns (explicit)

| Don't | Why |
|---|---|
| Import from `test_utils` in a new test | Forces back-coupling; defeats coexistence |
| Patch `test_utils` to keep an old test green | Restores the maintenance burden that killed the last attempt |
| Half-migrate a file (old + new tests in same file) | Worst possible state; mixed cleanup |
| `#[serial]` in a new test | Reveals isolation breakage; fix the harness instead |
| Use rexpect/PTY anywhere | The model that killed the last attempt |
| Assert exact stdout strings | Coupling we explicitly broke from |
| Set `std::env::set_var` from a test or helper | Process-global state; breaks parallelism |
| Spawn the ngit binary via `assert_cmd::Command` without setting the harness env | Misses relay-URL injection; test runs against hardcoded defaults |
| Add a "convenience" re-export of `test_utils` types into `test_harness` | Bridges defeat the boundary |
| `repo.git(["push", ...])` to a nostr remote | Misses the post-push 1s tick; flakes under parallel load. Use `Repo::nostr_push` instead. See "Timing rule" above. |

## Open questions (resolved during PR 1)

- Which `nostr-relay-builder` source to use for the vanilla relay
  (`with_relay`). Crates.io 0.44.x is the default for v1; revisit if
  the API gap matters.
- Whether scenario builders return a typed `Proposal` /
  `Repository` / etc. or just side-effect on the harness. Likely
  typed for ergonomics, but emerges from the first few migration
  PRs.
- Where to handle test-mode behaviour beyond relay configuration —
  spinner suppression, cache path overrides, etc. — that today rides
  on `NGITTEST`. Out of scope for v1; tracked separately.

### Resolved during design

- **Env-var schema**: one env var per role (`NGIT_RELAY_DEFAULT_SET`,
  `NGIT_RELAY_BLASTER_SET`, `NGIT_RELAY_SIGNER_FALLBACK_SET`,
  `NGIT_GRASP_DEFAULT_SET`). Simple, mirrors the existing field
  structure, no parser needed beyond `;`-split.
- **Test directory layout**: `tests/legacy/` for old (with explicit
  `[[test]]` entries in `Cargo.toml`), `tests/` for new.
- **ngit-grasp coupling**: binary only, no library import. Discovered
  via `$NGIT_GRASP_BIN` with sibling-clone fallback. CI gets the
  binary via a pinned `ngit-grasp` flake input on `flake.nix`; the
  dev shell exports `NGIT_GRASP_BIN` automatically.
- **Relay model**: vanilla relays (`with_relay`) for non-repo events
  via `nostr-relay-builder`; GRASP (`with_grasp_server`) for repo
  events and git data, as subprocess.

## References

- Audit reports informing this design (held in conversation history,
  not committed): legacy harness audit, `new-test-harness` branch
  review, ngit-grasp embeddability audit, main harness audit, env-var
  propagation verification.
- `pr/rename-split-relay-sets` (commit `d0f7f596`) — unmerged
  relay-set rename/split. Deliberately not a prerequisite for this
  harness; instead, this harness is a prerequisite for cleanly
  landing that kind of refactor in future.
- Commit `83b08861` — "replace broken ngit_list tests with
  ngit_pr_checkout"; established the non-PTY `std::process::Command`
  + `--json` pattern that this harness generalises.
- ngit-grasp's `tests/common/relay.rs` — port allocation and
  subprocess management pattern adopted here.
