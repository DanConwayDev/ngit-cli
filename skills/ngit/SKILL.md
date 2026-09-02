---
name: ngit
description: Provides commands and workflows for nostr:// git repositories, OCI container publication, and NIP-5A nsites using the ngit CLI and git-remote-nostr. Activates when working with nostr:// remotes or URLs, ngit commands, gitworkshop.dev repositories, Nostr CI status or workflow definitions, publishing OCI images through Nostr and Blossom, NIP-5A nsites, Blossom static-site publication, or generic collaboration requests such as opening an issue, creating or reviewing a PR, commenting, merging, or cloning. In a nostr repository it replaces GitHub/GitLab collaboration workflows and their APIs/CLIs.
license: CC-BY-SA-4.0
metadata:
  version: "1.14"
---

# ngit — Nostr Plugin for Git

ngit makes `clone`, `fetch`, `push` work with `nostr://` URLs and adds a CLI
for PRs, issues, repo management, OCI containers, and NIP-5A static sites over
the decentralised Nostr protocol.

- Install: `curl -Ls https://ngit.dev/install.sh | bash` (installs `ngit` and `git-remote-nostr`)
- Web UI: https://gitworkshop.dev

## How it works

**Nostr** is a decentralised protocol where users publish signed events to relays (simple servers anyone can run). There is no central authority — identity is a keypair, and data is replicated across many relays.

Git has two distinct layers that ngit separates:

- **Git state (refs)** — which commit each branch/tag points to — is published as signed events on Nostr relays. This is the source of truth for the repository.
- **Git data (objects)** — the actual commits, trees, and blobs — is stored on ordinary git servers (any server that speaks the git protocol).

When you `git fetch`, `git-remote-nostr` reads the current ref state from Nostr relays, then fetches the corresponding objects from the git server(s) listed in the repository announcement. Because the state lives on Nostr and the data can live anywhere, git servers are interchangeable — switching providers requires no coordination with contributors.

**Grasp servers** are a convenience: they combine a Nostr relay and a git server into a single hosted service (e.g. `relay.ngit.dev`). When `ngit init` publishes a repository announcement listing a grasp server, the grasp server automatically creates the git repository — no prior setup or account configuration required. You can use separate relays and git servers if you prefer.

## Key rules

- **`pr/` prefix is MANDATORY for PRs** — branch names for pull requests MUST start with `pr/` (e.g. `pr/my-feature`). A branch without this prefix is a plain git push and will never create a PR.
- **Always use `--json`** on `ngit` subcommands when reading output. It is a global option, so it works at any command position (for example, `ngit --json issue create` and `ngit issue create --json`). Stdout contains exactly one JSON document after the command finishes; relay updates and other human diagnostics stay on stderr. `git` commands do not support `--json`.
- **Use `--offline`** on all but the first `ngit` command in a session — reads from local cache instantly. `git fetch origin` also refreshes the cache.
- **Never construct NIP-05 addresses** (`user@domain`). Use the `npub1...` form unless a NIP-05 address was explicitly provided.
- **`<ID|nevent>`** accepts a `nevent1...` bech32 string, a 64-char hex event ID, or a unique hex prefix with an optional leading `#` (e.g. `#deadbeef`). Ambiguous prefixes fail and list the matches. Get IDs from `ngit pr list --json` or `ngit issue list --json`.
- **`--json` collaboration output uses `nevent1…` bech32** for `id` and `reply_to` fields. Use these values directly as `<ID|nevent>` arguments and in `nostr:` URI references. Container publication instead returns a raw-hex `event_id` plus the repository's canonical `naddr`.
- **Reference other issues/PRs/comments in `--body` using `nostr:` URIs** — e.g. `nostr:nevent1abc…` or `nostr:naddr1abc…`. Never paste raw hex IDs into body text. The `id` field from `--json` output is already a valid `nevent1…` string; prefix it with `nostr:` to form the URI. Example: `--body "Relates to nostr:nevent1abc…"`. ngit automatically converts these into the correct event tags.
- **Multiline files are safe with normal `ngit` text options, but not with `git push -o`.** For `ngit ... --body` or `ngit ... --description`, pass the file as one quoted argument: `--body "$(cat note.md)"`. For a Git push option, real newlines are forbidden; use literal `\n` only for a short inline value. Never convert a file into `-o description=...`.
- **Use `--signer <alias|npub|nostr-display-name>` to select a non-default stored identity for one `ngit` command.** For one Git command, use `git -c nostr.signer=<alias|npub|nostr-display-name> push ...`; this does not change the configured login. Do not export or pass an nsec merely to switch between configured accounts.
- **Check Nostr CI explicitly after pushes and when diagnosing test coverage.** Repository CI workflows live under `.ngit/act/workflows/`; do not infer CI success from a successful push, local validation, or files under another provider's workflow directory. Query the exact commit with `ngit ci status <COMMIT-ISH> --json` and inspect `ci.conclusion` rather than the top-level command `status`. See `reference/ci.md`.

## Detecting a nostr repo

```bash
git remote -v | grep -q 'nostr://'   # primary check — no cache needed
ngit repo --json --offline            # full metadata when needed
```

`ngit repo` always exits 0; `is_nostr_repo: false` can be a cold-cache false negative — if remotes show `nostr://`, run `git fetch origin` then retry. Full output includes roster fields plus `lead_source`, `lead_path`, `pending_actions`, and `health`. Follow an explicit forward with `ngit repo follow-lead`. "Invited" means the relationship is not reciprocal; an invited member's events are not authoritative until they accept. Moderators can manage issues and PRs but never publish repository state. See `reference/repo-settings.md` for settings, named membership actions, and the role model.

## Selecting the target repository

When a git repository has multiple `nostr://` remotes for different repositories, use global `--repo <REMOTE|NADDR|NOSTR-URL>` to select the target explicitly. Prefer the configured remote name:

```bash
ngit --repo upstream issue create --subject "Bug" --body "Details" --json
ngit pr --repo upstream list --json
```

Without `--repo`, ngit uses repository configuration and branch tracking to infer the target, and fails instead of guessing when the choice is ambiguous. Before signing an event, verify the `target repository: <naddr> (source: ...)` diagnostic on stderr.

## nostr:// URLs

```
nostr://<npub>/<identifier>
nostr://<npub>/<relay-hint>/<identifier>   # relay-hint is bare domain, e.g. relay.ngit.dev
```

Standard git commands work directly with these URLs — `git-remote-nostr` resolves them transparently. See `reference/repositories.md` for publishing and cloning.

## Command reference

Detailed command references live in `reference/*.md` in this skill's directory. Read the matching file before performing that slice of work:

| Task | Reference |
| ---- | --------- |
| Publish/clone repos, URL forms | `reference/repositories.md` |
| Publish NIP-5A static sites through Blossom | `reference/nsites.md` |
| Repository settings and membership | `reference/repo-settings.md` |
| Open, stack, review, merge PRs | `reference/prs.md` |
| Create/view/comment/close issues | `reference/issues.md` |
| Write CI workflows, install ngit in CI jobs, inspect runs, trust, failures | `reference/ci.md` |
| Publish OCI containers through Blossom and Nostr | `reference/containers.md` |
| Accounts, login, secrets | `reference/accounts.md` |
| Sync, flags, git config | `reference/sync-config.md` |
