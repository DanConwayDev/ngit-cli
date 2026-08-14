# Repositories — publish, clone, and URLs

Part of the ngit skill. Read this when publishing a repository, cloning one, or resolving `nostr://` URL forms.

## nostr:// URLs

```
nostr://<npub>/<identifier>
nostr://<npub>/<relay-hint>/<identifier>   # relay-hint is bare domain, e.g. relay.ngit.dev
```

Standard git commands work directly with these URLs — `git-remote-nostr` resolves them transparently.

## Publishing a repo

```bash
ngit init --name "My Project" --description "What it does" -d # uses user's preferred grasp server or falls back to defaults
ngit repo edit --description "New description"                   # update metadata
ngit repo --json --offline                                       # view repo info (check nostr_url field)
```

## Cloning

```bash
git clone nostr://<npub>/<relay-hint>/<identifier>   # preferred
git clone nostr://<npub>/<identifier>                # slower discovery, no relay hint
git clone nostr://user@domain.com/<identifier>       # NIP-05, only if given to you
```
