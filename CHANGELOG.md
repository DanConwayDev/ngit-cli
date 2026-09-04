# Changelog

All notable changes to ngit will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

**Summary:** Private repos via private GRASP service (GRASP-08), basic Buzz Git
support, portable local and remote signer management, stacked PRs, target
branches, a skill installer/upgrader, and NIP-5A site and OCI container
publication.

### Added

- `ngit release publish` can derive its version and source commit from one
  exact Git tag, so tagged CI builds need no positional arguments. Schema-1
  manifests accept Zapstore-compatible local APK `release_source`; ngit
  extracts package/version, SDK, certificate, and ABI metadata from the stable
  Blossom snapshot and rejects conflicts with declarative assertions.
- Schema-1 release manifests can declare Zapstore-style application metadata.
  Tracked local icons and screenshots publish through Blossom, while existing
  HTTP(S) image URLs remain direct references and are never downloaded.
- Add `ngit update`, `ngit update --check`, and exact-version selection backed
  by ngit's signed repository state and trusted NIP-82 release assets. Only
  receipted standalone Unix installations are replaced automatically; Nix,
  Cargo, and unreceipted installations receive non-mutating guidance. The
  canonical website installer template now shares this update path.
- Release manifests can set `release_notes: CHANGELOG.md` to publish the
  matching Keep a Changelog version section. Missing, duplicate, and empty
  sections fail rather than falling back to the complete changelog.
- Add `ngit nsite publish <DIRECTORY>` for publishing an already-built static
  site as a root or named NIP-5A manifest. The command snapshots files
  deterministically, confirms every unique blob on every selected Blossom
  server with batched BUD-11 authorization, then signs and publishes the
  manifest with local or remote signers. Blossom servers may be supplied
  explicitly, read with site metadata, fallback routing, and relay hints from
  nsyte's `.nsite/config.json`, or discovered from the account's kind-10063
  server list.
- Add `ngit container publish <OCI_LAYOUT>` for publishing verified OCI image
  layouts through Blossom and repository-bound Nostr kind-30624 state. The
  command uploads every reachable blob, preserves existing remote tags by
  default, publishes through repository relays, and supports NIP-42 and
  structured CI output.
- **Private repositories via GRASP-08**: discover private repositories through
  encrypted kind-10318 relay lists, then clone, fetch, push, and collaborate
  using NIP-42 relay authentication and repository-scoped NIP-98 Git HTTP
  credentials. Private repository events stay on repository relays instead of
  leaking through indexers, fallback relays, or account relay fanout. Copied
  repository URLs can also be classified as private from GRASP-08 NIP-11
  metadata before discovery begins.
- **Basic Buzz support**: clone Buzz repositories, view pull requests and their
  lifecycle status, and push new or updated pull requests and status changes
  through authenticated Buzz relay and Git transport. Normal branch and tag
  pushes are not supported, and Buzz pull-request comments cannot be viewed or
  published because Buzz uses kind 1 while ngit uses NIP-22 kind 1111.
- Stored accounts can be selected for one ngit command with global
  `--signer <npub|alias|profile-name>`, or for one Git command with
  `git -c nostr.signer=<account>`, without changing the configured login.
  `ngit account login <account>` reactivates retained credentials and
  `--alias` adds a portable signer name. An explicit selector that is missing,
  invalid, or ambiguous fails closed.
- Add portable one-shot and export paths for both signer types:
  `--nsec-file` reads a local key, while interoperable `--nbunksec` and
  `--nbunksec-file` reuse an established NIP-46 session. Secret-file targets
  must be small regular files and, on Unix, use mode `0400` or `0600`.
  `ngit account export-keys` returns the selected account's npub plus `nsec`
  for a local signer or `nbunksec` for a remote signer.
- `--json` is now a global option for all ngit commands. Machine-readable
  results are written to stdout only after command completion, diagnostics and
  progress stay on stderr, and mutating commands return useful event IDs and
  metadata. JSON output cannot be combined with interactive mode.
- Pull requests can target a non-default branch with `git push -o target-branch=<branch>` or `ngit send --target-branch <branch>`. Root PR events expose the target as an indexed `b` tag, and listing, viewing, updating, merging, and applied-status detection honor the immutable target; omitting the option retains default-branch behavior. `ngit merge` resolves an explicit target against the latest Nostr repository state, fetching the target commit without mutating tracking refs, so a stale local clone cannot merge onto old target history.
- Pull requests automatically select the unique most-advanced tip of the author's other open or draft PRs when it is in the new proposal's history and ahead of the target branch. Existing children follow later authorized parent updates after they are rebased, while stale children and ambiguous unrelated parents fail closed. `git push -o base=<commit|branch|event>` and `ngit send --base <commit|branch|event>` override inference for that publication; PR roots select their latest authorized update, specific PR-update events select their historical commit, and unique event-ID prefixes are accepted. Repeat an explicit historical base on later child updates to keep that deliberate pin instead of following the open parent lineage.
- Issue-resolution status events now identify the triggering commit and, when
  attribution is unambiguous, the proposal and merge commit that caused the
  status change.
- Add a multi-stage `Containerfile` for building a minimal Alpine-based ngit image, plus a CI smoke test that builds the image and runs `ngit --version`.
- Add an opt-in `native-tls-roots` build feature that trusts certificate authorities installed on the host alongside WebPKI roots, enabling WSS connections to relays and GRASP servers using private, corporate, or development CAs.
- `ngit repo --json` now exposes `selected_maintainer`, `confirmed_maintainers`, `invited_maintainers`, `lead_maintainer`, and the directional `maintainer_edges` alongside the backward-compatible full `maintainers` set.
- Add `ngit skill install`, `upgrade`, and `status` commands plus local and global reminder opt-outs for repository-managed coding-agent guidance. Installs add a slim versioned `SKILL.md` and eight on-demand `reference/*.md` guides to both Codex and Claude discovery paths, including static-site publication with `ngit nsite`, and append a compact pointer only to existing `AGENTS.md` or `CLAUDE.md` files that do not already mention ngit. Installs and upgrades preserve supported symlinks, protect modified or newer copies unless forced, and create distinct guidance-only commits; non-maintainers are advised to push the commit as a pull request, while `ngit init` leaves installation as an explicit suggested follow-up.
- Expand `--repo-relay-only` to all ngit commands that publish nostr events.
- Git server clone URLs can use installed `git-remote-<scheme>` helpers for listing, fetching, and pushing. Installing a helper is treated as consent for signed repository announcements to invoke it, subject to Git's protocol policy; recursive `nostr`, internal `fd`, and GRASP-reserved `ws`/`wss` schemes are not delegated.
- Global `--repo <REMOTE|NADDR|NOSTR-URL>` argument selects the target repository for repo-scoped operations (`send`, `issue`, `pr`, `repo`, `sync`, and every other command that resolves a repository coordinate). Available at any command position (`ngit --repo upstream issue create`, `ngit issue --repo upstream create`, `ngit issue create --repo upstream`). Value is first matched against configured remote names, then parsed as an naddr, then as a `nostr://` URL.
- Repo-coordinate resolution now prints a `target repository: <naddr> (source: ...)` diagnostic when publishing repo-scoped events, so an incorrect target is visible before the event is signed.
- Opportunistic Tor support for `.onion` relays and clone URLs. ngit uses an available SOCKS5 proxy from `NGIT_TOR_PROXY` or probes the common system Tor and Tor Browser ports (`127.0.0.1:9050` and `127.0.0.1:9150`). Unavailable onion entries fail immediately so they do not delay clearnet alternatives. See `docs/onion.md`.

### Changed

- JSON command-result envelopes now use top-level `command_status: "ok" |
  "error"` instead of the ambiguous `status` field or `ok` boolean. Nested
  fields continue to describe domain results, so a successful `ci status`
  query can pair `command_status: "ok"` with `ci.conclusion: "failure"`.
  Release, nsite, and container publication envelopes advance to format
  version 2 for this breaking schema change.
- `ngit merge` now accepts `--require-ci-trust`, matching `ngit pr merge`.
- Repository hosting flags now distinguish grasp-derived infrastructure from
  deliberate additions. `ngit init` uses `--additional-relay` and
  `--additional-clone`; `ngit repo edit` replaces whole-list
  `--grasp-server`, `--relay`, `--clone`, and `--hashtag` flags with repeatable
  `--add-*` and `--remove-*` actions. Repository identifiers can no longer be
  changed through `repo edit` because doing so creates a new coordinate.
- GRASP service URLs may include a non-root base path. Repository announcements, `nostr://` relay hints, Git push/fetch, and explicit GRASP-06 PR endpoints preserve the configured path.
- `ngit account whoami` now inventories every usable stored account from local, global, and system Git config, `credentials.json`, and the OS credential store; groups identities by npub; lists distinct local-key and remote-signer connections with their npub/alias selectors and connection-specific scope badges; and prints shared guidance for one-shot ngit or Git use, local/global activation, alias creation, and removing a local override. `ngit account list` is a visible alias, while `ngit account login <account>` adds a concise activation form alongside the existing `--signer` spelling. A non-secret account index makes newly stored OS-keyring identities discoverable without relying on platform-specific keyring enumeration.
- Logged-in accounts answer NIP-42 authentication challenges from repository
  relays, and from the account's inbox or outbox relays while publishing, but
  decline challenges from unrelated relays such as indexers and fallbacks.
- Now that ngit-grasp repository-relay synchronization has matured, repository state and collaboration events are fetched exclusively from relays declared by the repository. This makes repository relays the authoritative collaboration view and allows grasp servers to provide moderation in the future without filtered events being restored from user relays. URL hints, fallback relays, and announcement indexers are limited to repository announcements; user relays are limited to profile metadata, relay lists, and user GRASP lists. Publishing still fans out to relevant user relays unless `--repo-relay-only` or `nostr.repo-relay-only` is set.
- Open and draft PRs from other users are no longer downloaded as branches by
  default. `ngit pr checkout` opts an individual PR back in with its familiar
  shorthand-suffixed branch name and keeps later fetch, pull, and push tracking
  intact. The new default applies to fresh and existing clones; existing clones
  keep previously downloaded tracking refs until `git fetch --prune`. Set
  `nostr.auto-pr-branches=true` explicitly to restore automatic downloading.
- Global event caching now falls back to an in-memory cache when persistent storage is unavailable, allowing ngit to operate in restricted or sandboxed environments. Set `NGIT_CACHE_DIR` to select a writable persistent cache directory; repository caches remain strict and require the Git common directory to be writable.
- Upgrade NostrDevKit dependencies from the `0.45.0-alpha.2` prerelease series to the stable `0.45.0` release.
- Align maintainer terminology with gitworkshop: every pubkey in the directional maintainer graph has maintainer rights, while "invited" identifies an unreciprocated relationship rather than reduced authority. Reciprocal graph membership confirms co-maintainers, and a unique highest-listed confirmed maintainer is shown as a coordination-only lead. `ngit repo` describes whom each confirmed maintainer lists and, when informative, who invited an unconfirmed maintainer. Acceptance defaults now reciprocate the sole confirmed maintainer or unique lead, retaining the selected maintainer only for ambiguous non-interactive cases.
- Accepting co-maintainership (`ngit repo accept`, and the auto-accept that runs during push and status commands) no longer rewrites `nostr.repo` or the `origin` remote to point at the accepter's own coordinate. The coordinate a repository resolves from is the root of trust; re-rooting it on your own announcement — which always lists you as a maintainer — would make it impossible to observe the inviter removing you later. Resolution stays on the inviter's coordinate, so a removal surfaces naturally (for example as a refused push); only `ngit repo edit` / `ngit init` change the resolved coordinate deliberately. When `origin` is not a `nostr://` remote, `ngit repo accept` now prints how to add a nostr remote for the inviter's coordinate instead of claiming pushes will work.
- `ngit account login` and `ngit account create` now store local keys and typed remote-signer records in the OS credential store, falling back to a user-only file store in ngit's data directory when no OS store is available. With credential-backed storage, Git config keeps only public account-selection and alias metadata. Plaintext values in Git config remain supported indefinitely and are never rewritten by reads; run `ngit account login` to move an existing plaintext login into a credential store. The `nostr.secret-storage` Git config item, `NGIT_SECRET_STORAGE` env var, or `--secret-storage` flag on `ngit account login` / `create` selects `auto` (OS store then file store), `file`, or `git-config` (plaintext, the previous behaviour); when a secret cannot be stored, login fails with guidance instead of silently saving plaintext. `ngit account logout` keeps the stored secret — the store may hold the only copy of the key — and prints the `ngit account forget-keys <entry>` command that removes it; `ngit account logout --forget` does both in one step. Generated keys are no longer printed by `ngit account create`; use `ngit account export-keys` when the secret needs to be revealed deliberately. See `docs/credential-storage.md`.
- Repository-coordinate resolution now follows a documented priority: (1) explicit `--repo`, (2) `git config nostr.repo`, (3) current branch's tracked upstream if a `nostr://` remote, (4) `origin` if a `nostr://` remote, (5) sole remaining distinct nostr coordinate. When multiple distinct coordinates remain and none of the earlier rules match, ngit errors by default and prints how to disambiguate, instead of silently picking one at HashMap-iteration random. Interactive selection is offered only when `-i` is explicitly requested and uses deterministic (name-sorted) ordering.
- `ngit init` first-time use now recreates the existing `origin` as a remote named after the git server's domain (e.g. `github` for github.com; deeper hosts drop the public suffix and dash-join the rest, like `git-fiatjaf` for git.fiatjaf.com; name collisions get a numeric suffix) instead of discarding its URL entirely when `origin` is repointed at the nostr URL. Branch upstreams and existing tracking refs are untouched.
- `ngit init` now pushes git data and publishes the repository state in-process instead of spawning `git push` / `ngit sync` subprocesses. The state is cached and broadcast only after a Git server accepts the data and a relay accepts the event. A failure while pushing or establishing the state is a real error that reports that the announcement was already published and names the follow-up command (`git push -u origin <branch>` or `ngit sync`), instead of being downgraded to a warning.
- Repeat `ngit init` on a repository that already has a state event republishes it as a fresh event (identical refs, new event id) through the same acceptance-gated flow as a push, so relays and git servers newly added to the announcement receive the repository state immediately instead of only on the next `git push`. Establishing the state during init now requires at least one reachable git server.
- `ngit init` on a repository with a pre-existing reachable `origin` now records `refs/remotes/origin/*` remote-tracking refs for the branches covered by the published state, so ahead/behind reporting against the repointed nostr `origin` is correct before the first push.
- The remote helper no longer creates, updates or deletes `refs/remotes/<remote>/*` tracking refs itself: git's own transport layer performs those updates for every ref the helper reports `ok`, mapping the destination through `remote.<name>.fetch`. The helper's only remaining local ref bookkeeping is deleting legacy tag tracking refs written by old ngit versions. Two behaviours deliberately change to match vanilla git semantics: a remote whose `remote.<name>.fetch` refspec has been narrowed no longer receives tracking refs for pushed branches outside that refspec, and `git push <nostr-url>` with no configured remote now completes without the helper's previous bookkeeping error, writing no tracking refs.

### Fixed

- The `nostr://` Git remote helper now accepts raw commit object IDs on the
  source side of push refspecs, matching Git's normal behavior for commands
  such as `git push origin <oid>:refs/heads/recovery`. Branch and lightweight
  tag destinations are recorded in repository state without requiring a
  temporary local ref.
- Blossom presence checks and upload responses now treat a server's MIME type
  as representation metadata rather than blob identity. Content-addressed bytes
  can be reused by files with different media types, so an existing empty blob
  labelled `inode/x-empty` no longer blocks publication of an empty CSS file;
  hash, size, and descriptor URL validation remain strict.
- `ngit update` now recognizes signed prerelease events whose NIP-82 channel
  matches the first SemVer prerelease identifier (`rc.7` uses `rc`, `beta.2`
  uses `beta`) for both automatic and exact-version updates, while continuing
  to accept backward-compatible `main`-channel events. Stable releases remain
  restricted to `main`, and pending-release warnings now use the correct
  singular or plural verb.
- Remote-signer login no longer overwrites a different NIP-46 connection for
  the same npub before failing to persist Git config. A bare npub retains its
  default signer, while `--alias <name>` stores and selects additional signer
  sessions independently for login, push, and `account export-keys`.
  Credential-store alias values remain raw npubs so older ngit versions can
  continue reading them. If only one session remains and it was stored for an
  alias, either the npub or alias can still select it; multiple sessions with
  no default require an alias.
- `ngit account export-keys --secret` prints only the selected account's nsec
  or nbunksec, providing a direct human-readable export without a JSON wrapper
  or interactive menu.
- `ngit repo edit` now refreshes the maintainer's current announcement from
  every NIP-65 write relay before deriving a replacement. A cold cache can no
  longer let a stale indexer announcement make a targeted `--add-*` edit drop
  existing grasp servers, relays, clone URLs, hashtags, or membership data;
  incomplete account-relay reads fail before anything is signed or published.
- `ngit merge` now preserves staged, unstaged, and untracked changes across
  the merge commit and branch switch, including the staged/unstaged boundary.
  If those changes conflict with the merged target, ngit rolls the merge back
  and restores the original branch and worktree. Linked worktrees are
  supported when the command runs from the worktree that owns the target
  branch; if another worktree owns it, ngit refuses before changing either
  worktree or the shared target ref.
- `ngit init --defaults` no longer drops grasp hosting when `--additional-clone`
  or `--additional-relay` is supplied. Additional infrastructure supplements the
  account's preferred grasp servers (or ngit's defaults) instead of silently
  replacing them, so the announcement keeps its grasp-derived clone URLs and
  relays. Publishing without any grasp server is now stated explicitly with an
  empty value, `--grasp-server ""`, which also requires an additional relay and
  clone URL of its own. Republishing an announcement that genuinely declares no
  grasp servers no longer grafts the defaults onto it.
- `ngit init` and `ngit repo edit` refuse to publish a repository announcement
  whose relay or clone field would be empty, naming `--grasp-server`,
  `--additional-relay` and `--additional-clone` as the ways to supply the
  missing half. The check runs on the resolved announcement before anything is
  signed, so it covers every flag shape and repository configuration, including
  a metadata-only edit of an announcement that already lacks hosting — which
  previously republished the unusable announcement and then failed while
  pushing git data with no git server to connect to.
- Prevent TLS client initialization from panicking after Reqwest 0.13 selected
  AWS-LC alongside rust-nostr's Ring provider. Reqwest remains
  provider-neutral while ngit explicitly installs Ring before constructing
  either its application or library HTTP clients.
- Repository-conditioned global signer identities, aliases, and secret-storage
  policies selected through Git `includeIf` `gitdir` conditions are now
  resolved in repository context. Conditional signers work for
  `ngit account whoami` and ordinary signed commands, while
  `nostr.secret-storage` and legacy `nostr.credential-store` settings control
  login storage and plaintext hints.
- ngit now honours `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and `GIT_CONFIG_NOSYSTEM` when resolving Git config scopes outside the repository. libgit2 does not apply these to the config ngit opens, so a login stored, read or removed with `--global` (or any global-scope read, such as the secret-storage policy) previously went to `~/.gitconfig` regardless of the redirect, silently overwriting the real global login of anyone who sandboxes ngit with the documented variables.
- Keep `--json` output parseable by sending relay-fetch summaries such as `no updates` and `updates: ...` to stderr instead of stdout.
- `git push --force-with-lease` now works with `nostr://` remotes: matching leases authorize guarded non-fast-forward updates, while stale leases reject the push before a conflicting repository state can be published.
- `git push` of a `pr/` branch alongside branch or tag changes now reports the proposal ref as failed when no git server accepts the pushed git data, matching the branch/tag refs. Proposal events are broadcast to the repository and user relays only after a git server accepts the pushed data; previously the helper told git `ok` and recorded a `refs/remotes/<remote>/pr/<branch>` tracking ref in that total-failure case even though the proposal events were never broadcast.
- `git push` to a nostr remote now treats a git server that already has every requested change as a successful push target, so no-op pushes (re-pushing a tag that is already present, deleting an already-deleted branch) succeed instead of erroring, including under `nostr.nostate`. Git data is no longer pushed to a GRASP server whose paired relay did not accept the staged state event, and the locally cached repository state is only updated after a git server accepted the pushed data and a relay accepted the state event, so a failed or interrupted push leaves the previous state authoritative instead of caching an unpublished replacement.
- Git pushes that encounter libgit2's internal protocol assertion now retry
  through the system Git transport, allowing affected SSH servers such as
  tangled.org to accept the same push.
- Make ahead/behind commit walks topology-complete so merged side-branch commits with equal timestamps are not skipped, restoring every issue-resolution status generated from a multi-merge push.
- Fast successive repository and proposal updates now order reliably despite Nostr's whole-second timestamps. Repository state, announcements, and statuses use bounded nonce grinding with a timestamp fallback; GRASP now honors the lower-event-ID tie-break for same-second state replacements. Patch revisions and pull-request upgrades or updates remain strictly ordered by timestamp.
- `ngit init` first-time use no longer fails to publish git data for existing `origin` refs that were never downloaded locally (e.g. tags after a `--no-tags` or single-branch clone): the missing objects are now fetched from the origin by ref name before the repository state is signed, and refs whose objects still cannot be obtained are excluded from the state event instead of being advertised as oids no git server holds.
- Fix silent mis-targeting of repo-scoped events (`ngit send`, `ngit issue create`, `ngit pr *`, `ngit repo`, etc.) when a repository had multiple `nostr://` remotes with disagreeing coordinates. Previously the resolver iterated a `HashMap` and picked the first key it saw, ignored `nostr.repo`, and printed no diagnostic; the effect was that PRs and issues could be published against the wrong repository coordinate without warning. See the documented priority under "Changed".
- `ngit merge` run with no argument on a bare `pr/<name>` branch now falls back to matching the branch's tip commit against the published tips of open and draft PRs when the logged-in-author mapping finds zero or several candidates. A maintainer merging a contributor's PR from a hand-made bare branch, and a logged-out user merging their own, now resolve the PR instead of erroring; when several open PRs share both the branch name and the tip commit, merge still asks for an explicit event-id.

## [3.0.0-rc.7] - 2026-09-03

- Seventh v3 release candidate, preserving backward-compatible signer records,
  distinguishing multiple remote-signer connections for one identity, adding
  direct credential export, and publishing ngit's complete cross-platform
  release through NIP-82 from the GitHub release workflow.

## [3.0.0-rc.6] - 2026-09-03

- Sixth v3 release candidate, unifying resilient Blossom publication and
  compact progress reporting across releases, nsites, and containers; adding
  shared quiet output and CI-trust-gated merges; preserving dirty worktrees;
  and hardening repository hosting and patch validation.

## [3.0.0-rc.5] - 2026-09-01

- Fifth v3 release candidate, making automatic PR branch downloads opt-in,
  exposing issue edit history, improving release publication feedback and
  compatibility, hardening Windows credential writes, and avoiding private
  discovery requests for already-resolved public repositories.

## [3.0.0-rc.4] - 2026-08-31

- Fourth v3 release candidate, adding release-aware self-updates, project
  container manifests, changelog-derived release notes, and Zapstore-compatible
  application and APK metadata, with hardened Blossom placement verification
  and clearer viewer-relative CI trust labels.

## [3.0.0-rc.3] - 2026-08-29

- Third v3 release candidate, adding NIP-5A static-site and OCI container
  publishing and fixing maintainer state handoff and TLS provider selection.

## [3.0.0-rc.2] - 2026-08-29

- Second v3 release candidate, fixing the TLS initialization regression in
  rc.1.

## [3.0.0-rc.1] - 2026-08-29

- First v3 release candidate for upgrade, compatibility, and cross-platform
  packaging validation. Detailed v3 changes remain under Unreleased until the
  final 3.0.0 release.

## [2.6.3] - 2026-07-10

### Fixed

- `ngit init` now promptly reports actionable account setup guidance when no signer is configured, instead of repeatedly trying to prompt for an nsec.

## [2.6.2] - 2026-07-07

### Fixed

- `git push` to the default branch now never publishes duplicate PR merge/applied status events for PRs that are already marked applied; also improved merge detection now using the pre-push nostr repo state rather than git internals

## [2.6.1] - 2026-06-29

### Fixed

- Remove accidental extra relay calls to check for new ngit versions; version checks now only use update relays when already connected to them.

## [2.6.0] - 2026-06-26

### Added

- `ngit merge` merges a PR into the default branch as a no-ff merge commit (recording the PR nevent and author in the commit message) without pushing
- `ngit pr` and `ngit issue` commands that accept a PR/issue event ID now accept unique hex prefixes, with or without a leading `#`; ambiguous prefixes fail with the matching items listed
- `ngit init -u ...` / `--u ...` / `--upstream ...` publishes the informational NIP-34 `u` tag for subordinate forks, and `ngit repo` now displays/serializes existing `u` metadata without ever inventing it by default
- `ngit account connect` as an alias for `ngit account login -i` (interactive nostr connect login) [hzd149]
- Pushing commits that include issue-closing keywords (e.g. `fixes` / `resolves`) followed by #<hex-event-id-or-8-char-prefix> or nostr:nevent123 now auto-resolves referenced issues
- Added env-var and git-config options to increase libgit2 HTTP connect and per-socket I/O timeouts, helpful for large pushes with git servers that may be silent for longer than the default timeout; `NGIT_HTTP_CONNECT_TIMEOUT_MS` / `NGIT_HTTP_IO_TIMEOUT_MS` override `nostr.http-connect-timeout-ms` / `nostr.http-io-timeout-ms` for one-off commands — thanks to new contributor mstrofnone
- warn when newer ngit version available

### Changed

- Use git index relays along side relay hints to find repositories during cloning
- Bump dependencies: updated to rust-nostr v0.45.0-alpha.2 (required significant internal refactoring); semver-compatible lockfile updates across the board; dev-dependencies rstest 0.23 → 0.26 and mockall 0.13 → 0.14
- The interactive remote-signer login menu now has a single "bunker" option that shows the QR code and the nostrconnect:// connection string together while it waits for the signer to connect, and concurrently offers options to manually paste a `bunker://` url, change the signer relays, or cancel; previously these were separate menu entries gated behind ctrl+c [hzd149]

### Fixed

- Merge status detection now handles updated PRs correctly
- Better default branch and merge-base detection when force-pushing PRs
- `cargo install ngit` was failing to build due to an upstream dependency picking up an incompatible `git2` version
- Pushing a tag through a `nostr://` remote wrote a stray ref at `refs/remotes/<remote>/<tagname>` — git's remote-tracking branch namespace — so tags appeared as remote branches in `git branch -r`, IDE listings, completion, etc. The remote helper no longer writes a per-remote tracking ref for tags (the local `refs/tags/<name>` is git's single source of truth), and `ngit sync` sources tag push refspecs directly from the nostr state event oid. On the next push or `ngit sync`, any stray legacy entries are deleted automatically.
- Invited co-maintainers auto-accept maintainership during push and auto-update the remote to reflect their npub. This no longer breaks the push.
- `ngit send --in-reply-to` now errors before publishing an update to an existing PR when the signer is neither the proposal author nor a repository maintainer.
- Push reporting no longer returns `ok` before any git server has successfully received the git data.
- State events are no longer broadcast before the git data is successfully pushed: ngit now publishes the state event to GRASP servers using their purgatory support, pushes the git data, then only broadcasts to additional relays after at least one git server succeeds, so `ok` requires the latest state on at least one relay and the git data on at least one git server.
- `.onion` hosts supplied as a relay-hint segment in a `nostr://<npub>/<host>/<repo>` URL no longer get a stray `wss://` prefix, which previously made onion relays unreachable. Same fix applies to the `?relay=` query-string form.

### Removed

- NIP-05 signer-URL lookup removed from the remote-signer login flow; it was unused and added an extra step before reaching the bunker / QR-code screen

## [2.5.0] - 2026-05-26

### Changed

- `git push pr/<branch>` and `ngit send` now default to PR kind for new proposals when the repository has at least one GRASP server; previously only oversized commits (>60 KB) or those containing submodules triggered PR kind automatically
- When a PR cannot be pushed to the repository's GRASP servers, ngit uses GRASP-06 instead of creating a new personal-fork announcement; `ngit send --git-server` or `git push -o git-server=<url>` lets contributors target a custom git URL or GRASP server explicitly
- `ngit init` republishes now preserve unknown tags from the existing announcement instead of silently stripping them, so tags added by a future ngit version or third-party tool are never lost on republish; a yellow warning lists the carried-over tag names and `--clean` will remove them
- `ngit pr apply`, `ngit pr checkout`, and `ngit pr list` now consult git servers lazily — only when the needed commit is not already present locally — and share a single fetch helper; previously each command had its own near-duplicate fetch logic, and `checkout` fetched unconditionally even when the commit was already local — thanks to m0wer for the contribution
- `ngit pr checkout` (and the interactive path in `ngit pr list`) now tries the submitter-supplied clone URLs from the PR event as a fallback when the repo's declared git servers don't carry the PR tip, matching the existing behaviour in `ngit pr apply`; this fallback is disabled for passive `git clone`/`git fetch` paths to prevent a malicious submitter slowing down every repo operation
- Integration test harness completely rewritten for greater reliability and speed

### Fixed

- Force-pushing a rebased `pr/<branch>` produced a PR update event with a stale `merge-base` tag; the remote helper was forwarding the original stored tag instead of recomputing from the actual git topology; the merge-base is now always recomputed from `git merge-base` for every push type
- `ngit pr checkout` correctly checked out the PR as a local branch but left the working directory at its previous state
- `ngit pr checkout --force` on a patch-kind proposal failed with "failed to find parent commit" when the new revision depended on a commit not yet in the local repo; the fetcher now retrieves the parent from the git servers before applying
- `ngit send --in-reply-to <hex_event_id>` for a non-root reference (e.g. an issue mention) emitted an `["e", ...]` tag instead of the NIP-21 `["q", ...]` quote tag; hex and bech32 inputs now produce the same tag

## [2.4.4] - 2026-05-16

### Added

- `ngit sync --trust-server` (`-t`): when a git server is fast-forward ahead of nostr state, sync reports the affected refs and requires `--trust-server` to sign and publish an updated state event reflecting the server's commits; in non-interactive mode the update is auto-accepted
- `nostr.trust-server-domains` git config setting: semicolon-separated list of git-server hostnames that `ngit sync` automatically trusts when they are fast-forward ahead of nostr state, without requiring `--trust-server`; the new commits are also pushed to any other git servers; can be set globally (`git config --global nostr.trust-server-domains 'github.com;codeberg.org'`) or per-repository
- Diverged-branch detection in `ngit sync`: branches where the git server and nostr state have each made commits the other lacks are always reported with a concrete recovery command (`git fetch <url> <branch> && git push <nostr-remote> +<branch>`) and cannot be resolved with `--trust-server`

### Fixed

- fast-forward push to a `pr/` branch backed by a PR event (kind 1618) or PR update event (kind 1619) failed with "event is not a patch"; these events store the tip commit in a `"c"` tag rather than a `"commit"` tag, so `get_commit_id_from_patch` now checks the `"c"` tag for PR/PR-update events before falling back to the mbox content heuristic
- fast-forward push to a `pr/` branch that is based on an existing PR event produced a PR update event with an incorrect `merge-base` tag; the previous tip of the PR was used as the merge-base instead of the original branch divergence point; the fix reads the `merge-base` tag from the root PR event and forwards it unchanged into the PR update event
- patches containing submodule entries (mode 160000) now return an error from `create_commit_from_patch` instead of crashing the `git-remote-nostr` process with signal 11 (SIGSEGV); libgit2's `apply_to_tree` dereferences a null pointer on such entries rather than reporting an error, so the submodule diff is detected before calling into libgit2 and the proposal is skipped gracefully; `ngit send` and the `pr/` branch push path in `git-remote-nostr` now also detect submodule entries up-front and automatically use PR mode so the commits are pushed directly rather than sent as patch events

### Documentation

- strengthen `pr/` branch prefix requirement in ngit skill: add a mandatory key rule bullet and a CRITICAL callout in the PR workflow to prevent LLMs from omitting the prefix
- document in ngit skill that omitting `-o title=` / `-o description=` when pushing a single-commit PR is preferred — ngit uses the commit subject and body automatically
- clarify newline handling in ngit skill: `git push -o 'description=...'` requires literal `\n\n` (ngit's push-option parser converts them); `ngit send --description` requires `$'...\n\n...'` ANSI-C quoting since the shell passes the argument verbatim and does not interpret `\n` in double-quoted strings

## [2.4.3] - 2026-05-01

### Fixed

- when a repo has multiple `nostr://` remotes sharing the same identifier, relays could return state events authored by maintainers of the _other_ remote; without filtering, the newest event won regardless of author, pointing refs at the wrong commits; state event candidates in `run_list` are now filtered to maintainers of the current remote's repo announcement

## [2.4.2] - 2026-04-28

### Fixed

- when submitting a PR (via `--force-pr` or when the existing proposal is already a PR kind), repository GRASP servers were never tried when pushing proposal refs; a URL normalisation mismatch (`repo_grasps` holds normalised hostnames but the comparison was made against full clone URLs) meant the candidate server list was always empty, so every submission fell through to the fork-creation / personal GRASP server fallback path instead of pushing directly to the repository's own GRASP servers

## [2.4.1] - 2026-04-22

### Fixed

- `fatal` errors during clone/fetch when an open PR's git data isn't available on the repository's specified git servers

## [2.4.0] - 2026-04-10

### Added

- git worktree support (a3b0bf6) - thanks to new contributor m0wer

### Fixed

- more robust patch parsing and gracefully handle errors (7a36aed, e1dd109, 6a2245d)
- panic when cloning a bare `nostr://npub/identifier` URL with no relay hints (f3a6ae8)
- repository identifiers containing reserved characters (e.g. spaces, emoji) are now percent-encoded in `nostr://` clone URLs and GRASP HTTP paths, per [NIP-34](https://github.com/nostr-protocol/nips/pull/2312)
- gracefully handle errors identifying potential PR merges on push (3daf61e)

## [2.3.0] - 2026-03-05

### Added

- **Issue management**: new `ngit issue` subcommand group (`list`, `view`, `create`, `close`, `resolved`, `reopen`, `comment`, `label`) for creating and viewing NIP-34 issues and posting NIP-22 comments
- **PR comments and viewing**: `ngit pr view` shows full PR details with all comments in chronological order; `ngit pr comment` posts a NIP-22 comment
- **NIP-32 labels**: apply hashtag labels to issues and PRs via `ngit issue label` / `ngit pr label`; labels from kind-1985 events are merged with inline `t` tags (author and maintainer only)
- **Set subject**: `ngit pr set-subject` / `ngit issue set-subject` — update the displayed title of a PR or issue after the fact via a NIP-32 kind-1985 `#subject` label event (author or maintainer only)
- **Cover notes** (kind 1624, experimental): attach a summary or context note to a PR or issue via `ngit pr set-cover-note` / `ngit issue set-cover-note`; displayed in place of the description in `view` output; designed to be pinned to the top of long threads
- **Repo-only relays**: new `nostr.repo-relay-only` git config key; when `true`, nostr events are sent only to the repository's own relays, skipping personal write and default relays; enable with `git config nostr.repo-relay-only true` or `ngit init --repo-relay-only`
- **`ngit account whoami`**: show the currently logged-in account(s); improved detection of credentials set at the system git config level (`/etc/gitconfig`)
- **SKILL.md**: AI agent skill file for working with ngit repositories

### Fixed

- `ngit pr checkout` now requires `--force` when the local branch has diverged from the proposal branch
- `ngit issue list` missing `--comments` flag added
- NIP-22 comment compliance fixes; `--reply-to` flag added for threaded replies on issues and PRs
- Login local config takes precedence correctly when posting comments

## [2.2.3] - 2026-02-27

### Fixed

- Regression introduced in 28ad5440: `ngit sync` crashed with "invalid refspec refs/remotes/origin/v1.4.4^{}:refs/tags/v1.4.4^{}" on repos with annotated tags; `RepoState::try_from` now retains `^{}` peeled-tag entries in state, but the sync refspec builder did not skip them; fixed by guarding all three iteration sites in sync.rs and `identify_remote_sync_issues` in list.rs; also corrected the always-false logic bug in `invalid_nostr_state_ref`

## [2.2.2] - 2026-02-27 [YANKED]

### Added

- `ngit sync --force` now republishes the state event with a fresh timestamp even when no refs have changed, allowing users to repair repos with a corrupt or incomplete state event (e.g. missing `^{}` peeled refs for annotated tags) without needing to push a new ref
- git server push option passthrough, enabling `-o secret-scanning.skip` for grasp servers
- `ngit sync` now publishes the current state event to grasp server relays that are missing it or have a stale version before attempting git pushes, preventing rejections; per-relay state visibility is captured during the nostr fetch and surfaced via `FetchReport::state_per_relay`
- Fetch filters now request kind-5 deletion events for cached state and repo announcement events by `#e` tag (NIP-09), in addition to the existing `#a`-tagged filter; ensures deletions of these events are received even from clients that do not embed a repo coordinate in their deletion event
- `KIND_PULL_REQUEST` (kind 1618) event IDs are now included in `proposal_ids` when building fetch filters, so kind-5 deletion events that only `#e`-tag a PR Kind event (without an `#a` repo coordinate tag) are fetched and applied; previously deleted PR Kind events remained in the local cache and continued to appear as remote refs
- `FetchReport` now tracks and displays a count of kind-5 deletion events received (e.g. `"1 deletion"` in the fetch summary)
- `ngit account login` nostrconnect flow now shows current signer relays and allows changing them
- `ngit account login --bunker-url` - specify bunker URL for non-interactive nostrconnect login

### Fixed

- Annotated tags missing from `git-remote-nostr` list output; peeled `^{}` refs were stripped when parsing the nostr state event, so git could not resolve the tag to a commit and `git fetch --prune` deleted it; existing repos with affected state events are self-healed on the next push
- Fallback signer relays updated: replaced `nsec.app` with `bucket.coracle.social` and `nos.lol` for nostrconnect resilience
- `merge-base` tag in PR events generated by `git push` of a `pr/` branch was set to the parent of the PR tip instead of the actual base commit; multi-commit PRs showed only 1 commit when applied via `ngit apply`
- `git-remote-nostr` list now advertises the newest state event whose OIDs are all confirmed present on a git server or locally, rather than unconditionally using the latest nostr state event; this prevents catastrophic fetch/clone failures when a state event was published before the corresponding git push completed
- Tag tracking refs written with wrong path (`refs/remotes/origin/refs/tags/v1.0.0` instead of `refs/remotes/origin/v1.0.0`) after a push via `git-remote-nostr`, causing `ngit sync` to fail with "src refspec does not match any existing object" when syncing tags
- Annotated tag tracking refs stored with the peeled commit OID instead of the tag object OID after a push via `git-remote-nostr`; this caused `ngit sync` to push the wrong object to grasp servers, which rejected it because the nostr state event referenced the tag object OID
- `ngit sync --verbose` detailed per-relay view not shown; `--verbose` flag was ignored due to a logic error in `send_events`
- `ngit sync` using wrong refspec source (`refs/remotes/origin/refs/heads/master` instead of `refs/remotes/origin/master`), causing sync to fail with "src refspec does not match any existing object"
- State event publish failures silently swallowed during push; summary now shows `"Published to X/N relays (failed: relay1 relay2)"` instead of unconditional success message
- Grasp servers whose internal relay did not receive the state event are now skipped during push, with a clear warning; push fails with an error message when no servers remain
- New state event is now removed from the local cache and the previous state restored when a push fails entirely, so retries start from a clean baseline

## [2.2.1] - 2026-02-25

### Fixed

- IPv6 connection failures with Happy Eyeballs (RFC 8305)

## [2.2.0] - 2026-02-20

### Changed

- **AI-friendly commands** all non-interactive by default
  - Add `--defaults/-d` flag for sensible defaults
  - Add `--force/-f` flag to bypass safety guards
  - Add `--interactive/-i` flag to enable prompts
- **Simplify CLI output**
  - Add `--verbose/-v` flag for detailed output
  - show fetch/publish report if taking longer than 5s
- **Default relay updates**: replace `nos.lol` with `relay.ditto.pub` (nos.lol requires NIP-42 auth even for reads); add `relay.ditto.pub` as second default signer relay for nostrconnect resilience
- **Grasp server readiness**: poll grasp servers for readiness instead of a fixed 5-second wait

### Added

- **`ngit repo` subcommand group**: new command group for repository management operations
- **Auto-accept co-maintainership on push**: when pushing to a repo where you are listed as a maintainer but have not yet published an announcement, ngit now automatically publishes your announcement (using your grasp servers or the trusted maintainer's as fallback) and provisions your grasp server instead of failing
- `ngit account login --signer-relay` - specify custom relays for nostrconnect (auto-prefixes with `wss://` if no scheme)
- `ngit checkout <id>` - checkout a proposal branch by event-id or nevent
- `ngit apply <id>` - apply proposal patches to current branch; supports both patch and PR-format proposals
- `ngit account create` - create a new nostr account
- `ngit list --json` - output proposals as JSON
- `ngit list --status` - filter by status (open,draft,closed,applied)
- `ngit list --offline` / `ngit checkout --offline` / `ngit apply --offline` - skip relay fetching and use local cache
- `ngit init --hashtag` - specify repository hashtag
- Push options for PR title/description: `git push --push-option=title="..." --push-option=description="..."`
  - Multiline support: use `\n` for newlines in values (e.g. `--push-option=description='line1\n\nline2'`). Use single quotes to prevent shell interpretation. Use `\\n` for a literal backslash-n.
- `ngit list` output improvements: show active filter, 'To view' hint, and yellow hint lines

### Removed

- `--blossoms` flag from `ngit init` (removed from GRASP spec)
- `relay.nostr.band` from default relays (relay is now offline)

### Fixed

- Compatibility with clients that omit optional patch tags
- Branch tracking setup when checking out proposals (c85ca81)
- Handle existing local branch that is behind when checking out PR (1be46b4)
- Preserve progress bars on relay errors during clone (b8716ed)
- Show help menu when `ngit` is run without arguments (972e220)
- Various output formatting and wording improvements

## [2.1.0] - 2025-11-18

### Added

- **Adaptive relay timeouts**: Relay timeout is now 45 seconds initially, then 7 seconds based on successful connections for faster operations
- **Async git list refs operation**: Made git ref list async and include sync report inline for better visibility
- **Improved fetch reporting**: Fetch report now shows even when successful in git remote helper for better transparency

### Fixed

- **CLI output improvements**:
  - Fixed line deletion during fetch operations
  - Fixed dim coloring in CLI output
  - Fixed out of sync grasp server CLI output
- **Tag handling**: Don't attempt to fetch annotated tags that are already available locally
- **Fetch reporting**: Fixed cached profile events being incorrectly shown as new

### Changed

- Updated to rust-nostr v0.44

## [2.0.1] - Fix Account Creation on NixOS

### Fixed

- **NIP-46 bunker url privacy** tag bunker pubkey rather than user pubkey to communicate with bunker
- **Create account** show nsec for manually setting nostr.nsec git config when not able to set global git config

## [2.0.0] - Pull Request support

### Breaking Changes

- **SSH Key Authentication in nostr:// URLs**: The user field in nostr git URLs (e.g., `nym1@ssh/npub123/identifier`) is now treated as an SSH key file location rather than an SSH user. SSH key can be specified as a file within `~/.ssh` (e.g., `~/.ssh/nym1`) or as a full/relative path. Most git servers expect the SSH user to be 'git', so specifying a different SSH key is the idiomatic way to use different credentials.

### Added

- **Pull Requests Support**: Introduced complete PR functionality for large contributions that would be too big for relays as patches:
  - Generate PR events for oversized patches automatically
  - Support PR updates and PR as patch revision
  - List open/draft proposals on repo relays/servers as `pr/*` branches and all proposals as `refs/pr/*` and `refs/pr/pr-by-id/head`
  - Push PRs to custom clone URLs with auto-fork creation fallback
  - Add `--force-pr` and `--force-patch` flags for manual control
  - Full NIP-34 compliance with `merge-base` tags

- **NIP-22 Status Events Support**: Read and process NIP-22 style status events for proposals and PRs

- **ngit sync command**: New command to synchronize git servers with nostr state
  - Optional `--force` flag for forced synchronization eg deleting refs on non-GRASP servers
  - `--ref-name` parameter to limit sync to a single reference

- **ngit init improvements** (simple model for non-grasp servers):
  - Use user's grasp list for defaults instead of hardcoded options
  - List and allow selection/deselection of non-grasp servers
  - Check and fetch origin refs when missing locally
  - Publish state event and sync when existing origin matches tip

- Allow specifying non-default SSH key in `nostr://` address

### Fixed

- **Git server timeouts**: More robust timeout enforcement in both ngit binary and remote helper
- **Annotated and lightweight tags**: Proper handling and pushing of all tag types
- **nostr:// URLs with NIP-05**:
  - Fixed URLs with NIP-05 addresses without local part
  - Allow NIP-05 domain without `_@` prefix
- **Sync and fetch improvements**:
  - Don't fetch tags already available locally
  - Fetch refs missing locally before sync, fail gracefully
  - Include all valid nostr state (was incorrectly filtering)
- **Repository state**: Only use state and announcements from authorized maintainers
- **Status events**: Only use status events from author and maintainers
- **Grasp server detection**: Fix to ensure no SSH fallback when not needed
- **NIP compliance updates**:
  - Fix `t` tag: `revision-root` → `root-revision` (NIP-34)
  - Fix mention marker → `q` tag (NIP-10 update)
- **Error handling**: Capture more errors when updating refs
- Suppress warnings for poorly formatted proposals (only show to maintainers/author)

### Changed

- Updated to latest rust-nostr v0.43
- Updated gitworkshop.dev URL format (now uses nevent)
- Removed blossom from grasp server detection (removed from grasp spec)
- Print event description before publishing for clearer terminal UI

## [1.7.4] - 2025-07-16

### Fixed

- Apply nip46 breaking changes as remote signers remove nip04 support
- Apply relay connection timeout once, instead of per request batch
- Add git server timeouts
- Bump all dependencies

## [1.7.3] - 2025-06-20

### Changed

- Rename ngit-relay to grasp

### Fixed

- Always include HEAD in state event

## [1.7.2] - 2025-06-18

### Fixed

- Fix clone when HEAD isn't in nostr state event

## [1.7.1] - 2025-06-17

### Fixed

- Add support for `git://` clone urls

## [1.7.0] - 2025-06-03

### Added

- Quality-of-life features for ngit-relay users
  - Detect ngit-relays and only attempt using unauthenticated http protocols
  - Better sync and less errors as nostr is the only way to push
- Overhaul `ngit init`
  - Add simple / advanced mode
  - Add support for ngit-relays
  - Specify blossom servers
  - Sensible defaults
- Add resiliency - push to all maintainer's relays and git servers
- Require additional maintainers to publish announcements before pushing
- Allow users to specify fallback relays see `ngit --customize`
- Add show npub command

### Fixed

- Use newest state event found, rather than oldest
- More resilient builds for platforms and distros

## [1.6.3] - 2025-05-12

### Fixed

- Fallback to http protocol if ssh is unavailable

## [1.6.2] - 2025-05-06

### Added

- Add event description for remote signing process

### Fixed

- Fix custom ports use for git servers

### Changed

- Bump all dependencies to latest major versions

## [1.6.1] - 2025-04-02

### Changed

- Build binaries for more OSes

## [1.6.0] - 2024-12-20

### Added

- Overhaul and simplify login experience
- Add `account` api with `login`, `logout` and `export-keys` commands
- Add sign up feature targeted at users new to nostr
- Support nip05 addresses in nostr git urls (e.g., `nostr://dan@gitworkshop.dev/ngit`)
- Rework `ngit init` to make on-boarding more intuitive with simpler questions and more guidance
- Expand merge types that automatically update PR status when pushed

### Changed

- Don't create `maintainers.yaml` for new repos but continue to support it for existing projects
- Remove ngit `pull`, `push` and `fetch` api to nudge users to use native git commands with git plugin
- Bump dependencies (e.g., rust-nostr to v0.37)

### Fixed

- Fix `ngit account login` from outside of a git repository
- Add QR code border
- Make `ngit list` prompts more intuitive

## [1.5.3] - 2024-11-12

### Fixed

- Fix remote signing as nip46 update has breaking changes
- Auth to relays on requests
- Fix `pr/` branch name prefix issue
- Fix `ngit init` error when remote added before initiation
- Don't blast initiation events as munity blaster is no more
- When git-remote-nostr called directly show help instead of error

### Changed

- Bump rust-nostr to v0.36
- Replace sqlite with lmdb due to rust-nostr deprecation

## [1.5.2] - 2024-09-24

### Added

- Login via nip46 QR code
- Enable login directly in git plugin
- Add resilience to git plugin so that a poorly formatted PR will gracefully fail

## [1.5.1] - 2024-09-20

### Changed

- Git plugin reports on event broadcasting

## [1.5.0] - 2024-09-18

### Added

- New nostr url format that works better for MacOS users: `nostr://<*protocol>/<npub123>/<*relay-hint>/<identifier>` (\*optional)
- Status updates during clone, push and fetch
- Intelligent protocol selection and fallback
  - Unless unusual protocol specified in clone url it will try in this order:
    - fetch: https unauth, ssh, https
    - push: ssh, https auth
  - Save successful protocol in git config so it is tried first next time
  - Enable override from nostr url (will only use this protocol)
- Enable building binaries via nix

### Changed

- Refactor into lib and bin structure
- Bump dependencies

## [1.4.6] - 2024-09-13

### Fixed

- Fix `ngit push` and `ngit pull` when on a pr branch not in the format `pr/<branch-name>(<8-chars-from-id>)`

## [1.4.5] - 2024-08-30

### Added

- When clone url is ssh use auth for `list` and `fetch` as they are required
- When clone url is ssh, fallback to https so read events don't always require auth

### Fixed

- Stop asking for git server credentials when pushing `pr/` branch
- Fix `no repo events at specified coordinates` error via rust-nostr v0.34.1 upgrade

## [1.4.4] - 2024-08-27

### Added

- Include git plugin in release zip

## [1.4.3] - 2024-08-27

### Fixed

- Fix clone using nostr url

## [1.4.2] - 2024-08-20

### Fixed

- Only maintainers can push normal branches / tags

## [1.4.1] - 2024-08-20

### Fixed

- Fix pushing tags in git-remote-nostr

## [1.4.0] - 2024-08-20

### Added

- Add git-remote-nostr binary

## [1.3.1] - 2024-07-25

### Fixed

- Fix(init): update maintainers.yaml if identifier or relays have changed

## [1.3.0] - 2024-07-24

### Added

- NIP-46 remote signing (from Amber, etc)
- `list` breaks down proposals by status
- Local cache in `.git` to enable viewing proposals offline and reuse by other git clients
- Introduced `fetch` to download recent proposals
- Improved repo selection and handling of multiple maintainers
- Unique branch names for proposals to prevent name conflicts
- Login to different npubs for different repositories
- Store login details in git config so they can be reused by other git clients ran locally
- Add NIP-31 alt tags to events
- Add euc marker per NIP-34 tweak

### Fixed

- Ensure repo events of all maintainers are tagged in proposals
- Stop filtering out very large patches

## [1.3-beta1] - 2024-07-05

### Added

- Beta release for testing

## [1.2.1] - 2024-05-14

### Fixed

- Fix ngit init support for multiple maintainers

## [1.2.0] - 2024-05-14

### Added

- `ngit send --in-reply-to` tag any nostr notes and npubs in proposals
- `ngit send` link to proposal on gitworkshop

### Changed

- Remove unreliable relay.f7z.io from default relay set

## [1.1.2] - 2024-04-16

### Added

- Improve relay timeout behaviour
- Improve reliability via dependency upgrade
- Build via nix in ci

### Fixed

- Various reliability improvements

## [1.1.1] - 2024-03-08

### Fixed

- Fix stack overflow bug when origin remote doesn't exist

## [1.1.0] - 2024-03-08

### Added

- ngit send - improve proposal commit

## [1.0.0] - 2024-02-29

### Changed

- Major version to indicate breaking changes, not stability

## [0.1.2] - 2024-01-31

### Added

- Early release improvements

## [0.1.1] - 2024-01-26

### Added

- Early release improvements

## [0.1.0] - 2024-01-23

### Added

- Initial minor release

## [0.0.2] - 2023-05-23

### Added

- Early development release

## [0.0.1] - 2023-05-21

### Added

- Initial release
