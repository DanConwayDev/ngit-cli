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
 ├─── ngit subprocess (Command, env={NGIT_USER_INDEX_RELAYS=ws://127.0.0.1:54321, ...})
 │      ↓ may spawn:
 │      └─── git subprocess (inherits env)
 │             └─── git-remote-nostr subprocess (inherits env via execve)
 │
 └─── git subprocess (Command, env={...})
        └─── git-remote-nostr subprocess (inherits env)
```

Three observations underpin this:

1. **`std::process::Command::env()` is per-child, not process-global.**
   Multiple tests can set different `NGIT_USER_INDEX_RELAYS` values
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
| Relay/git ports | OS-assigned via `:0` bind in each grasp instance (ngit-grasp pattern); discovered after start |
| Relay URL config | `Command::env("NGIT_USER_INDEX_RELAYS", ...)` etc. per spawn |
| `NGITTEST` | Still set per spawn for fallback code paths; new env vars override the hardcoded localhost defaults |
| Working tree | `tempfile::TempDir` under `std::env::temp_dir()` — not `current_dir()` |
| `GIT_EXEC_PATH` (the dir containing `git-remote-nostr`) | Per-test tempdir; binary copied in once per test |
| User identity | Per-test `nostr::Keys::generate()` — no shared test keys |

Nothing is process-global. Nothing requires `#[serial]`.

### Relay-injection mechanism: per-spawn env vars

`d0f7f59` already added git-config support for relay configuration in
`src/lib/client.rs::Params::new()`, but tests bypass it because of the
`NGITTEST=true` gate. Rather than enable that path, we add an env-var
override that lives *inside* the existing `NGITTEST` branch:

```rust
// inside Params::default(), in the NGITTEST=true branch:
user_index_relays: env_relay_list("NGIT_USER_INDEX_RELAYS")
    .unwrap_or_else(|| vec![
        "ws://localhost:8051".to_string(),
        "ws://localhost:8052".to_string(),  // legacy hardcoded fallback
    ]),
// (similarly for git_nostr_index_relays, default_signer_relays,
//  grasp_default_servers)
```

`env_relay_list` reads a `;`-separated list of URLs from the named env
var. When unset, behaviour is unchanged from today. When set, it
overrides. This is **purely additive** — old tests, which set
`NGITTEST=TRUE` and no env vars, see no behavioural change.

The harness sets these env vars on every `Command` it spawns. Git
inherits them; git-remote-nostr inherits them via `execve`. The chain
is identical to how `NGITTEST` reaches the helper today, just with more
keys.

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

### Test API shape (sketch)

```rust
#[tokio::test]
async fn clone_over_grasp_succeeds() -> Result<()> {
    let harness = Harness::builder()
        .with_grasp_server("user")        // role label, not positional
        .with_grasp_server("repo")
        .build().await?;

    let user_keys = harness.generate_user_keys();
    let repo_id  = harness.publish_repo_announcement(&user_keys, &["repo"]).await?;

    let repo = harness.fresh_repo()?;     // TempDir, configured
    let out  = repo.git(["clone", &harness.nostr_url(repo_id), "."]).run()?;

    assert!(out.status.success(), "clone failed: {}", out.stderr_lossy());

    let snapshot = repo.snapshot()?;
    assert!(snapshot.refs.contains_key("refs/heads/main"));

    let events = harness.grasp("repo").events(
        Filter::new().kind(Kind::GitRepoAnnouncement).author(user_keys.public_key())
    ).await?;
    assert_eq!(events.len(), 1);

    // harness shuts down all grasp instances on drop
    Ok(())
}
```

Key shapes:

- **`Harness::builder()`** — fluent builder, async. Returns when all grasp
  instances are listening.
- **Role labels** — strings (`"user"`, `"repo"`, `"fallback"`, ...), not
  positional indices. Maps to env-var roles inside `client.rs`.
- **`harness.fresh_repo()`** — `TempDir`-backed `git init`d repo, with
  git-config pre-populated to match the harness's relay roster.
- **`repo.git([...])`** / **`repo.ngit([...])`** — fluent `Command`
  wrappers; inherit env from the harness automatically.
- **`run()`** returns a struct with `status`, `stdout: Vec<u8>`,
  `stderr: Vec<u8>`, plus convenience accessors. Never panics; tests
  decide what's an assertion.
- **`repo.snapshot()`** — returns a serializable struct: refs (name → oid),
  HEAD, config keys of interest, working tree status. Diffable across
  before/after.
- **`harness.grasp(role).events(filter)`** — queries the grasp instance's
  event store via a real nostr REQ over websocket. NIP-01 filter
  matching, no mock shortcuts.
- **`Drop`** — shuts down all grasp subprocesses. No `shutdown()` closure
  to remember.

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

## Required `src/` changes

### PR 1: additive env-var override in `client.rs`

Inside `Params::default()`, in the existing `NGITTEST=true` branch,
read four env vars before falling back to the hardcoded localhost
defaults:

- `NGIT_USER_INDEX_RELAYS`
- `NGIT_GIT_NOSTR_INDEX_RELAYS`
- `NGIT_DEFAULT_SIGNER_RELAYS`
- `NGIT_GRASP_DEFAULT_SERVERS`

Each is parsed as a `;`-separated list of URLs. Empty/unset → legacy
behaviour.

~30 lines, no API change, no breaking change to old tests. The
mechanism reuses the chain already certified by `NGITTEST` itself.

### Future (not blocking)

- Remove the `NGITTEST` gate around git-config relay reading in
  `Params::new()` (~7 lines). Lets git-config-based injection also
  work in tests. Strictly an improvement; not required for v1.
- Replace remaining `NGITTEST` branches in `src/` (spinners, cache
  path, fallbacks for other settings) with proper config injection.
  Larger refactor; tracked separately.
- Embed ngit-grasp as a library. Requires upstream additions:
  `embed::start()`, `bind :0` native, shutdown signal, embedded
  `Config` builder. Triggered if subprocess cost becomes the
  bottleneck.

## ngit-grasp dependency

- **No upstream changes required for v1.** Subprocess pattern matches
  ngit-grasp's existing test approach.
- **Imported as a git dependency** (not on crates.io). Pinned to a
  specific rev; both projects on `nostr-sdk` 0.44.1.
- **License compatible** (both MIT).
- The `ngit-grasp` binary must be built before tests run. The harness's
  `Harness::builder()` resolves the binary path via a build-script or
  config (TBD during PR 1). Two viable approaches:
  - Build ngit-grasp as a workspace member of a "test fixtures"
    workspace and reference its `cargo_bin` output
  - Require an env var (e.g. `NGIT_GRASP_BIN`) pointing to a
    pre-built binary, with a build script as a convenience

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

### PR sequence

**PR 1 — Foundation:**
- New `test_harness` crate skeleton (port allocator, grasp instance
  manager, `Harness` builder, `Repo` fixture, snapshot helper)
- Additive env-var override in `src/lib/client.rs::Params::default()`
- One lighthouse test: clone-over-grasp, in `tests/v2/clone.rs` (or
  similar — exact path TBD during PR 1)
- Old harness/tests UNTOUCHED
- CI runs both
- Mergeable in isolation, small, additive, low risk

**PR 2 — Freeze declaration:**
- Add `#![doc = "DEPRECATED — frozen, do not modify. New tests use \
  `test_harness`."]` to `test_utils/src/lib.rs`
- Update `AGENTS.md` to mandate the rules above
- Mergeable in isolation

**PR 3-N — Rolling migration:**
- Each PR migrates one logical area (init, push, send, pr_checkout,
  fetch). Rough ordering:
  1. `tests/git_remote_nostr/main.rs` — clone flow
  2. `tests/ngit_init.rs` — init + grasp announcement
  3. `tests/git_remote_nostr/push.rs` — push rstest groups
  4. `tests/ngit_send.rs` — PR send flow
  5. `tests/ngit_pr_checkout.rs` — already non-PTY, retarget setup
     onto scenario builders
  6. `tests/git_remote_nostr/fetch.rs`
  7. `tests/ngit_login.rs` — interactive parts dropped, non-interactive
     paths covered
  8. `tests/git_remote_nostr/list.rs`
- Each PR adds scenario builders to `test_harness` as that area
  demands them.
- Each PR deletes the migrated old file in the same commit.

**PR Final — Bury the body:**
- Delete `test_utils` crate
- Delete `tests/v2/` prefix if used (rename to `tests/`)
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

## Open questions (resolved during PR 1)

- Exact location of new tests: `tests/v2/` prefix, sibling
  `tests-v2/` crate, or in-place rename of `tests/` files? Resolved
  by file naming once PR 1 lands.
- How `test_harness` resolves the `ngit-grasp` binary path
  (build-script vs env var vs workspace member).
- Whether the env-var schema uses one var per role
  (`NGIT_USER_INDEX_RELAYS=...`) or one structured var
  (`NGIT_TEST_RELAYS=user:...,repo:...`). Per-role is simpler;
  keeping it unless a reason emerges to merge.
- Whether scenario builders return a typed `Proposal` /
  `Repository` / etc. or just side-effect on the harness. Likely
  typed for ergonomics, but emerges from the first few migration
  PRs.

## References

- Audit reports informing this design (held in conversation history,
  not committed): legacy harness audit, `new-test-harness` branch
  review, ngit-grasp embeddability audit, main harness audit, env-var
  propagation verification.
- Commit `d0f7f596` — relay-set rename and split; added git-config
  reading for relay configuration.
- Commit `83b08861` — "replace broken ngit_list tests with
  ngit_pr_checkout"; established the non-PTY `std::process::Command` +
  `--json` pattern that this harness generalises.
- ngit-grasp's `tests/common/relay.rs` — port allocation and
  subprocess management pattern adopted here.
