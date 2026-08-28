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
ngit repo --json --offline                                           # view repo info (check nostr_url field)
```

## Cloning

```bash
git clone nostr://<npub>/<relay-hint>/<identifier>   # preferred
git clone nostr://<npub>/<identifier>                # slower discovery, no relay hint
git clone nostr://user@domain.com/<identifier>       # NIP-05, only if given to you
```

## Settings and membership

Read `reference/repo-settings.md` before changing hosting, metadata, maintainers,
moderators, or the selected lead. It explains the distinction between
grasp-derived and additional infrastructure, targeted edit actions, state
republication, and the maintainer model.

```bash
ngit repo accept --json                       # accept a co-maintainer invitation
ngit repo accept --grasp-server <url> --json  # …and also host the git data there
ngit repo leave --json                        # end your own role and republish
ngit repo follow-lead --json                  # retain history and follow the lead
```
