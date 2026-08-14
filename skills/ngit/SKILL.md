---
name: ngit
description: Provides commands and workflows for nostr:// git repositories using the ngit CLI and git-remote-nostr. Activates when working with nostr:// remotes or URLs, ngit commands, gitworkshop.dev repositories, or generic collaboration requests such as opening an issue, creating or reviewing a PR, commenting, merging, or cloning. In a nostr repository it replaces GitHub/GitLab collaboration workflows and their APIs/CLIs.
license: CC-BY-SA-4.0
metadata:
  version: "1.5"
---

# ngit — Nostr Plugin for Git

ngit makes `clone`, `fetch`, `push` work with `nostr://` URLs and adds a CLI for PRs, issues, and repo management over the decentralised Nostr protocol.

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
- **Always use `--json`** on `ngit` commands when reading output — far easier to parse than human-readable text. `git` commands do not support `--json`.
- **Use `--offline`** on all but the first `ngit` command in a session — reads from local cache instantly. `git fetch origin` also refreshes the cache.
- **Never construct NIP-05 addresses** (`user@domain`). Use the `npub1...` form unless a NIP-05 address was explicitly provided.
- **`<ID|nevent>`** accepts a `nevent1...` bech32 string, a 64-char hex event ID, or a unique hex prefix with an optional leading `#` (e.g. `#deadbeef`). Ambiguous prefixes fail and list the matches. Get IDs from `ngit pr list --json` or `ngit issue list --json`.
- **`--json` output uses `nevent1…` bech32** for all `id` and `reply_to` fields (not raw hex). Use these values directly as `<ID|nevent>` arguments and in `nostr:` URI references.
- **Reference other issues/PRs/comments in `--body` using `nostr:` URIs** — e.g. `nostr:nevent1abc…` or `nostr:naddr1abc…`. Never paste raw hex IDs into body text. The `id` field from `--json` output is already a valid `nevent1…` string; prefix it with `nostr:` to form the URI. Example: `--body "Relates to nostr:nevent1abc…"`. ngit automatically converts these into the correct event tags.
- **Multiline files are safe with normal `ngit` text options, but not with `git push -o`.** For `ngit ... --body` or `ngit ... --description`, pass the file as one quoted argument: `--body "$(cat note.md)"`. For a Git push option, real newlines are forbidden; use literal `\n` only for a short inline value. Never convert a file into `-o description=...`.
- **Use `--signer <alias|npub|nostr-display-name>` to select a non-default stored identity for one `ngit` command.** For one Git command, use `git -c nostr.signer=<alias|npub|nostr-display-name> push ...`; this does not change the configured login. Do not export or pass an nsec merely to switch between configured accounts.

## Detecting a nostr repo

```bash
git remote -v | grep -q 'nostr://'   # primary check — no cache needed
ngit repo --json --offline            # full metadata when needed
```

`ngit repo` always exits 0; `is_nostr_repo: false` can be a cold-cache false negative — if remotes show `nostr://`, run `git fetch origin` then retry. Full output includes `nostr_url`, `maintainers`, `selected_maintainer`, `confirmed_maintainers`, `invited_maintainers`, `lead_maintainer`, `maintainer_edges`, and `grasp_servers`. "Invited" means the relationship is not reciprocal; it does not mean the maintainer lacks authority.

## Selecting the target repository

When a git repository has multiple `nostr://` remotes for different repositories, use global `--repo <REMOTE|NADDR|NOSTR-URL>` to select the target explicitly. Prefer the configured remote name:

```bash
ngit --repo upstream issue create --subject "Bug" --body "Details"
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
| Open, stack, review, merge PRs | `reference/prs.md` |
| Create/view/comment/close issues | `reference/issues.md` |
| Accounts, login, secrets | `reference/accounts.md` |
| Sync, flags, git config | `reference/sync-config.md` |
