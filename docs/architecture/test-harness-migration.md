# Test Harness Migration Plan

Supplement to [`test-harness.md`](./test-harness.md). The harness design
document specifies *what* the new harness looks like. This document
specifies *how* the rolling migration off `test_utils` and `tests/legacy/`
proceeds, which legacy tests are retained, and the scenario builders that
unlock the next migration step.

Read `test-harness.md` first.

## Guiding principles

1. **Preserve regression coverage; shed coupling.** Every legacy test
   exists because something used to break. A test is dropped only when it
   asserts on a contract we have explicitly decided to stop testing
   (interactive prompt rendering, exact CLI strings), never just because
   it is awkward to migrate.
2. **Force the kind, never infer it.** Every scenario builder that
   publishes a proposal does so with `--force-pr` *or* `--force-patch`
   explicit on the command line. Tests must remain green when the
   default-kind heuristic in `src/bin/ngit/sub_commands/send.rs` (and
   `src/bin/git_remote_nostr/push.rs` for `pr/` branches) changes
   underneath them — that is the entire point of the migration.
3. **rstest where it preserves signal, not where it reduces test count.**
   See "rstest discipline" below. The default answer is one
   `#[tokio::test]` per scenario.
4. **Each migrated test is deleted from legacy in the same commit that
   adds its replacement.** This is the hard "no half-migrated state"
   rule from `test-harness.md`, restated at the *test* granularity, not
   the *file* granularity: a legacy file may keep its remaining,
   not-yet-migrated tests while the file shrinks PR-by-PR. The forbidden
   state is "new test exists *and* the legacy test it replaced still
   exists" — that is what produces double-counted coverage and silent
   regressions when the legacy version is the one that breaks.
5. **PR scope is "a logical group of tests", not "a whole file".**
   Large legacy files (`ngit_send.rs` at 39 tests, `push.rs` at 28,
   `ngit_init.rs` at 35) are split across multiple PRs to keep session
   context tractable. The migration sequence below is therefore numbered
   by *group*, not by file; one file can appear across several entries.

## rstest discipline

Use `#[rstest]` **only** when both conditions hold:

- The cases share *identical* setup state — same harness, same scenario,
  same published events.
- Every case is a **read-only assertion on that captured state**: tag
  shape, event count, ref oid, JSON field. No case mutates the repo, the
  relays, or any subsequent case's view of them.

The canonical fit is the per-tag assertion explosion in legacy
`ngit_send.rs::cover_letter_tags` / `patch_tags`: 19 tests today, each
asserting on one tag of one event from one shared scenario. One
`#[rstest]` function with 19 `#[case]` rows preserves the per-tag failure
report while building the scenario once.

Do **not** use rstest when:

- Cases run further commands (`ngit send`, `git push`, ...) — those
  mutate event-store state and interact with each other.
- Cases need different harness compositions (different relay roster,
  different grasp count).
- The "parameter" is a setup choice (`include_cover_letter: bool`,
  `force_patch: bool`) rather than a post-hoc inspection. Write two
  named `#[tokio::test]` functions; the reader can tell them apart.

Per-rstest-case isolation requires a fresh `Harness` per case. The
fixture should publish the scenario *and* return the captured assertion
data plus the live harness — same shape as legacy
`TwoBranchesScenario` / `DeleteBranchScenario` in `push.rs`. `#[once]`
fixtures are deferred until measured startup cost forces the issue.

## Scenario builders

The migration is gated on these landing in `test_harness/src/scenarios.rs`.
All take `&PublishedRepo` from the existing `Harness::publish_repo` so
they compose with the maintainer/contributor distinction already there.

### `publish_pr` — PR-kind proposal via `ngit send --force-pr`

```rust
pub struct PublishPrOpts {
    /// Defaults to "feature-{n}" where n is monotonic per-PublishedRepo.
    /// No `pr/` prefix — `ngit send` does not require one, and we want
    /// the branch name to be observable in events independently of any
    /// `pr/` convention the remote helper may apply.
    pub branch: Option<String>,
    /// Files created on the branch and committed individually, one per
    /// commit. Defaults to two commits adding "t3.md" and "t4.md".
    pub commits: Vec<(String, String)>,
    /// Mandatory in opts: `ngit send --force-pr` will not synthesise one
    /// without `--defaults`, and we do not want the default to leak into
    /// scenarios.
    pub title: String,
    /// Mandatory for the same reason as `title`.
    pub description: String,
    /// Optional `--in-reply-to` reference (event id / nevent / npub /
    /// nprofile). Used by tests that exercise proposal-revision and
    /// mention-extraction flows.
    pub in_reply_to: Option<String>,
}

pub struct PublishedPr {
    pub event_id: EventId,
    pub author_pubkey: PublicKey,
    pub branch_name: String,        // e.g. "feature-1"
    pub commits: Vec<String>,       // oids in chronological order
    pub tip: String,                // last commit oid
    pub root_event: Event,          // captured KIND_PULL_REQUEST event
}
```

Implementation: `git checkout -b <branch>` from `main`, commit the files,
then
`ngit send HEAD~N --force-pr --title <t> --description <d> [--in-reply-to ...]`
against the maintainer or contributor's clone.

**Force-flag discipline:** `--force-pr` is mandatory and non-negotiable.
The scenario must produce a `KIND_PULL_REQUEST` event regardless of how
the default-kind heuristic in `src/bin/ngit/sub_commands/send.rs:236-243`
or the `git push -u pr/branch` path in
`src/bin/git_remote_nostr/push.rs` evolves. This decoupling is the entire
reason we are building scenario-based fixtures instead of reusing
`cli_tester_create_proposals`.

**Scenarios do not exercise `git push -u pr/<branch>`.** That path's
default kind is a behaviour we want to be free to change. Tests that
specifically pin the contract of *what kind `git push pr/branch`
produces* are written hand-rolled against the current default, and
updated in lock-step when the default changes. Those tests are a small,
identifiable group (today: `push_new_pr_branch_creates_proposal` and its
push-option siblings in legacy `push.rs`); every other test consuming a
"there is an open proposal" precondition uses `publish_pr` and gets the
forced kind.

### `publish_patch_series` — patch-kind proposal via `ngit send --force-patch`

```rust
pub struct PublishPatchSeriesOpts {
    pub branch: Option<String>,         // default: "feature-{n}" (no pr/ prefix)
    pub commits: Vec<(String, String)>, // default: two t-file commits
    pub cover_letter: Option<(String, String)>,
    pub in_reply_to: Option<String>,    // event id / nevent / npub etc.
}

pub struct PublishedPatchSeries {
    pub author_pubkey: PublicKey,
    pub branch_name: String,
    pub commits: Vec<String>,
    pub tip: String,
    pub cover_letter_event: Option<Event>,
    pub patch_events: Vec<Event>,
}
```

Implementation: branch + commit as above, then
`ngit send HEAD~N --force-patch [--title ... --description ... | --no-cover-letter] [--in-reply-to ...]`.

**Discipline:** `--force-patch` is mandatory. Without it, the kind
ngit picks is determined by `are_commits_too_big_for_patches` /
`do_commits_contain_submodules` / heuristic in `send.rs:236-243`. Tests
asserting on `Kind::GitPatch` must pin the kind explicitly.

### `publish_three_open_proposals` — `cli_tester_create_proposals` replacement

```rust
pub async fn publish_three_open_proposals(&self, repo: &PublishedRepo)
    -> Result<[PublishedPr; 3]>;
```

PR-kind by default. Authored by a fresh contributor identity (the
canonical "someone other than the maintainer submits proposals" shape).
Used by `fetch.rs`, `list.rs`, `pr_checkout.rs` migrations — all of which
need *open proposals exist* rather than caring which kind.

A patch-kind sibling exists for the narrow case where a test
specifically asserts patch behaviour:

```rust
pub async fn publish_three_open_patch_proposals(&self, repo: &PublishedRepo)
    -> Result<[PublishedPatchSeries; 3]>;
```

### `publish_repo_with_two_grasp_servers` — fan-out / failover setup

```rust
pub async fn publish_repo_with_two_grasp_servers(&self, opts: PublishRepoOpts)
    -> Result<(Repo, PublishedRepo)>;
```

Calls `ngit init --grasp-server <a> --grasp-server <b>` so both grasps
receive the announcement and both bare repos receive the initial push.
Used for `push.rs` fan-out + failover migrations. Requires
`HarnessBuilder::with_grasp_server("repo-a").with_grasp_server("repo-b")`.

### `arrange_init_state_{a..e}` — `ngit_init.rs` migration helpers

The five-state matrix in `legacy/ngit_init.rs` (fresh / coordinate-only /
my-announcement / co-maintainer / not-listed) is too distinctive to bury
inside a generic `publish_repo`. Add one helper per state, returning the
publisher `Repo` plus whatever metadata the subsequent `ngit init` call
needs. Defer until the `ngit_init.rs` migration PR — do not speculatively
build all five up-front.

## Migration sequence

Each row is one PR. Every PR adds the listed scenario builder(s) **and**
removes from `tests/legacy/` exactly the tests it has replaced, in the
same commit. Large legacy files (`ngit_send.rs`, `push.rs`,
`ngit_init.rs`) are split across multiple groups; the legacy file stays
in place with its remaining tests, and only disappears (along with its
`[[test]]` entry in `Cargo.toml`) when the *last* test is migrated out.

| # | Adds to `test_harness` | Migrates | Legacy file action |
|---|---|---|---|
| 1 | `publish_pr`, `publish_patch_series`, `publish_three_open_proposals` | `git_remote_nostr/fetch.rs` (1 test) | File emptied → delete file |
| 2 | (none — re-uses #1) | `ngit_pr_checkout.rs` (15 tests, mechanical) | File emptied → delete file + `[[test]]` entry |
| 3 | `publish_three_open_patch_proposals`, state-event manipulation helper | `git_remote_nostr/list.rs` (5 tests) + `git_remote_nostr/main.rs` (helpers + 1 trivial test) | Both files emptied → delete + remove `legacy_git_remote_nostr` `[[test]]` entry |
| 4a | (none new) | `ngit_send.rs` — `cover_letter_tags` + `patch_tags` groups (~19 tests as ~2 rstest functions) | Delete migrated tests; file shrinks |
| 4b | (none new) | `ngit_send.rs` — `when_no_cover_letter` + `root_proposal_*` + `in_reply_to_*` groups (~10 tests) | Delete migrated tests; file shrinks |
| 4c | (none new) | `ngit_send.rs` — `non_interactive_validation` (rstest arg-combos) + `when_commits_behind` non-interactive replacements + remaining keepers (~6 tests) | File emptied → delete + `[[test]]` entry |
| 5a | `publish_repo_with_two_grasp_servers` | `push.rs` — `two_branches_in_batch` + `delete_one_branch` groups (~7 tests via shared scenarios) | Delete migrated tests; file shrinks |
| 5b | (none new) | `push.rs` — proposal-merge status events, `push_2_commits_to_existing_proposal`, `force_push_creates_proposal_revision` (~5 tests) | Delete migrated tests |
| 5c | (none new) | `push.rs` — `push_new_pr_branch_*` family + force-push-with-cover-letter (~6 tests; this is the group that **deliberately** exercises `git push pr/<branch>` and stays coupled to its current default) | File emptied → delete + `[[test]]` entry |
| 6a | `arrange_init_state_a_fresh`, `arrange_init_state_b_coordinate_only` | `ngit_init.rs` — state-a + state-b groups (~9 tests) | Delete migrated tests |
| 6b | `arrange_init_state_c_my_announcement` | `ngit_init.rs` — state-c group (~9 tests) | Delete migrated tests |
| 6c | `arrange_init_state_d_co_maintainer`, `arrange_init_state_e_not_listed` | `ngit_init.rs` — state-d + state-e groups (~12 tests) | File emptied → delete + `[[test]]` entry |
| 7 | (none new) | `ngit_login.rs` — file deleted in full, not migrated (see "ngit_login dropped wholesale" below) | File deleted; no `[[test]]` entry existed |
| Final | — | — | Delete `test_utils/` crate, `tests/legacy/` directory, remaining `[[test]]` block in `Cargo.toml` |

Order rationale: PR 1 is the smallest end-to-end proof; PR 2 is the
biggest gain-per-LoC (already non-PTY); PR 3 unlocks state-event
manipulation primitives that 4 reuses; the `4*`, `5*`, `6*` groups
deliberately split heavy files so no single agent session exceeds a
reasonable context budget. PR 7 deletes the login file wholesale — see
"ngit_login dropped wholesale" below.

### ngit_login dropped wholesale

The 26 tests in `tests/legacy/ngit_login.rs` are deleted in full rather
than migrated. Rationale:

1. **The file was already dead.** No `[[test]]` entry in `Cargo.toml`
   (see lines 78–83 there) — `cargo test` has not been running it since
   the legacy freeze. Deleting it removes zero coverage from the
   currently-running suite.
2. **~21 of 26 are pure PTY/dialoguer interaction.** `CliTester`,
   `expect_choice`, `expect_input`, `-i` flag — explicitly banned by the
   harness rules. These would be dropped under any migration plan.
3. **The remaining ~5 non-interactive tests are already covered.**
   `test_harness/src/scenarios.rs` runs
   `ngit account login --local --nsec ...` in every `PublishedRepo`
   flow (see `scenarios.rs:393, 399` and dozens of sites). Every PR,
   send, fetch, push, and clone test in `tests/` depends on a successful
   login; if `--nsec` login broke, the entire suite would break before
   `ngit_login.rs`'s replacement ever ran.
4. **The one genuine gap is `--password` / ncryptsec.** No coverage
   exists in `tests/` or `test_harness/` for the password-encrypted-key
   storage path. This is left as a deliberate follow-up: if/when
   ncryptsec storage breaks, write a focused lighthouse test against
   the *then-current* contract (e.g. `tests/account_login_password.rs`),
   not a migration of a frozen PTY-heavy test that asserts on the old
   dialoguer prompt shape.
5. **`invalid_nsec_param_fails_without_prompts` asserts on an exact
   error string** (`"Error: invalid nsec parameter\r\n\r\nCaused
   by:\r\n    Invalid secret key\r\n"`) — banned by the no-exact-stdout
   rule. Cannot be migrated as written; the underlying "non-zero exit
   on invalid nsec" contract is trivially recoverable in a future
   lighthouse if needed.

PR 7 is therefore a pure deletion with no replacement tests added.

### Note for the `5*` PRs (push migrations)

Every push to a nostr remote in a migrated test **must** go through
[`Repo::nostr_push`](../../test_harness/src/repo.rs), not
`repo.git(["push", ...])`. The `nostr_push` wrapper runs the push and
then sleeps one whole unix second so the next event-publishing
operation in the test lands in a strictly later `created_at` second
than the auto-generated kind-30618 state event the push just emitted.
Skipping it produces a roughly 30% flake rate where the next publish
hits the relay's "this event is deleted" check_id path on a same-
second collision — see `docs/architecture/test-harness.md` §
"Timing rule" for the full chain. The previously-flaky
`tests/list_state.rs::state_event_takes_precedence_over_advanced_git_server_state`
is preserved as the witness; if it starts failing again the rule has
been violated somewhere.

## Keep / drop summary

The detailed per-file matrix lives in the migration PRs themselves. The
headline totals (after rstest-aware reclassification):

|  | Tests in legacy file | Kept (migrated) | Dropped |
|---|---|---|---|
| `ngit_send.rs` | 39 | ~35 | 4 (3× exact-stdout `check_cli_output`, 1× pure dialoguer confirm-rendering) |
| `git_remote_nostr/push.rs` | 28 | ~24 | 4 (exact-stdout `prints_git_helper_ok_respose`, redundant proposal-merge variants) |
| `ngit_init.rs` | 35 | ~30 | ~5 (pure dialoguer error rendering with no flag bypass) |
| `ngit_login.rs` | 26 | 0 | 26 (entire file dropped — see "ngit_login dropped wholesale") |
| `ngit_pr_checkout.rs` | 15 | 15 | 0 |
| `git_remote_nostr/list.rs` | 5 | 5 | 0 |
| `git_remote_nostr/fetch.rs` | 1 | 1 | 0 |
| `git_remote_nostr/main.rs` | 1 | 0 | 1 (clone-runs-fetch, already covered by `clone_grasp.rs`) |
| **Total** | **150** | **~110** | **~40** |

Categories of drop, exhaustively:

1. **Exact-stdout rendering** (`check_cli_output`, `prints_git_helper_ok_respose`, `cli_show_rejection_with_comment`) — banned by harness rules. The underlying behaviour (events published, git refs updated, exit code) is asserted elsewhere.
2. **Pure dialoguer prompt rendering** (`asked_with_default_no`, multi-select expectations, login choice prompts) — the prompt itself is the contract under test; no non-interactive equivalent exists. Where `src/` has a flag bypass, the test is migrated as a flag-bypass case instead.
3. **Redundant variants** (three proposal-merge tests that exercise the same status-event-issuance path with different merge styles; per-ref rstest cases that duplicate a HashSet assertion). One canonical case kept; siblings dropped.

## Definition of done

- Every `[[test]]` entry in `Cargo.toml` is gone.
- `tests/legacy/` does not exist.
- `test_utils/` is removed from the workspace.
- `cargo test` runs the harness-based tests only, in parallel, with no
  `#[serial]` markers and no PTY-driven assertions.
- The four `NGIT_*_SET` env-var paths in `src/lib/client.rs` may then be
  promoted out of the `NGITTEST=TRUE` branch (deferred follow-up).
