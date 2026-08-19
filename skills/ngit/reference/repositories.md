# Repositories — publish, clone, and URLs

Part of the ngit skill. Read this when publishing a repository, cloning one, resolving `nostr://` URL forms, or managing maintainers, moderators, and roles.

## nostr:// URLs

```
nostr://<npub>/<identifier>
nostr://<npub>/<relay-hint>/<identifier>   # relay-hint is bare domain, e.g. relay.ngit.dev
```

Standard git commands work directly with these URLs — `git-remote-nostr` resolves them transparently.

## Publishing a repo

```bash
ngit init --name "My Project" --description "What it does" -d --json # uses user's preferred grasp server or falls back to defaults
ngit repo edit --description "New description" --json               # update metadata
ngit repo --json --offline                                       # view repo info (check nostr_url field)
```

## Cloning

```bash
git clone nostr://<npub>/<relay-hint>/<identifier>   # preferred
git clone nostr://<npub>/<identifier>                # slower discovery, no relay hint
git clone nostr://user@domain.com/<identifier>       # NIP-05, only if given to you
```

## Members and roles

NIP-34 gives repository members three roles:

- **lead** — coordinates the project. Co-maintainers list only themselves and the lead in their announcements, so the lead can change the roster unilaterally. The lead has no extra authority in ngit beyond that convention.
- **co-maintainer** — full maintainer rights: publish repository state, merge PRs, manage issues.
- **moderator** — can manage issues and PRs (status changes, labels, comments) but can NEVER publish repository state or merge. ngit recognises moderators but has no command yet to assign one.

Membership is reciprocal. Being listed by a maintainer is only an **invitation**; your events are not authoritative until you publish your own announcement. An assigned moderator is likewise invited until they acknowledge the role.

```bash
ngit repo accept --json                       # accept a co-maintainer invitation
ngit repo accept --grasp-server <url> --json  # …and also host the git data there
ngit repo leave --json                        # end your own role and republish
```

`ngit repo leave` republishes your announcement with your role recorded as ended; per NIP-34 your own record takes precedence over other members' listings of you. It fails with a distinct error if you only hold an unaccepted invitation or unacknowledged moderator assignment (nothing to end) or already left.

Designate the lead when publishing:

```bash
ngit init --lead-maintainer <npub> --json
```

Naming yourself emits you as lead and keeps your full maintainer listing. Naming someone else follows NIP-34: your announcement then lists only you and the lead, and `--other-maintainers` beyond the lead is rejected. If the collapse would strip authorized-maintainer status from a pubkey your current announcement lists (no cover from the lead's own announcement), ngit refuses and names the affected pubkeys; `--force` overrides.

Inspect roles with `ngit repo --json --offline`. `members` contains one object per member:

- `role`: `lead` | `co-maintainer` | `moderator`
- `status`: `confirmed` | `invited` (an assigned-but-unacknowledged moderator is `invited`)
- `source`: `role_tag` (NIP-34 indexed role tags) | `maintainers_tag` (deprecated fallback listing) | `implicit` (implied by authoring an announcement)

`moderators` lists all assigned moderators and `confirmed_moderators` the acknowledged subset; the flat maintainer fields (`maintainers`, `confirmed_maintainers`, `invited_maintainers`, `lead_maintainer`, `maintainer_edges`) are unchanged.
