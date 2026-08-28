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
ngit repo follow-lead --json                  # retain history and follow the lead
```

`ngit repo leave` republishes your announcement with your role recorded as ended; per NIP-34 your own record takes precedence over other members' listings of you. It fails with a distinct error if you only hold an unaccepted invitation or unacknowledged moderator assignment (nothing to end) or already left.

Change one maintainer relationship at a time:

```bash
ngit repo edit --add-maintainer <npub> --json
ngit repo edit --remove-maintainer <npub> --json
ngit repo edit --lead-maintainer <npub> --json
ngit repo edit --acknowledge-maintainer-change <npub> --json
```

The first add by a sole maintainer makes that publisher lead automatically.
Use `--no-lead-maintainer` with every add or remove in a deliberately leadless
repository. A handover requires the proposed lead to publish the complete
current roster first. State collisions and same-identifier component joins
fail before publication; `--force` is reserved and does not bypass them yet.

Inspect roles with `ngit repo --json --offline`. `members` contains one object per member:

- `role`: `lead` | `co-maintainer` | `moderator`
- `status`: `confirmed` | `invited` (an assigned-but-unacknowledged moderator is `invited`)
- `source`: `role_tag` (NIP-34 indexed role tags) | `maintainers_tag` (deprecated fallback listing) | `implicit` (implied by authoring an announcement)

`moderators` lists all assigned moderators and `confirmed_moderators` the acknowledged subset. `lead_source` and `lead_path` explain resolution; `pending_actions` and `health` provide machine-readable follow or repair guidance.
