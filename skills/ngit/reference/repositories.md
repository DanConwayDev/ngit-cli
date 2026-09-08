# Repositories — publish, clone, URLs

Read when publishing or cloning a repository or resolving `nostr://` URL
forms. Guide: https://ngit.dev/repositories (hosting choices, migrating from
a forge, mirrors, private repositories).

## URLs

```
nostr://<npub>/<identifier>
nostr://<npub>/<relay-hint>/<identifier>   # relay-hint is a bare domain, e.g. relay.ngit.dev
nostr://<user>@<domain>/<identifier>       # NIP-05, only when explicitly provided
nostr://<domain>/<repository-path>         # NIP-AD: the full /path is sent URL-encoded to /.well-known/nostr.json?path=
```

## Publish

```bash
ngit init --name "My Project" --description "What it does" --defaults --json   # preferred grasp servers, else ngit defaults
ngit repo --json --offline                                                     # metadata, nostr_url, roster
```

Hosting flags, metadata edits, and membership: `reference/repo-settings.md`.

## Clone

```bash
git clone nostr://<npub>/<relay-hint>/<identifier>   # relay hint skips discovery
git clone nostr://<npub>/<identifier>
git clone nostr://user@domain.com/<identifier>       # NIP-05, only if given to you
git clone nostr://ngit.dev/ngit.git                  # NIP-AD bare-domain path
```

Open and draft PRs are not fetched as branches unless `nostr.auto-pr-branches`
is `true`; `ngit pr checkout <ID|nevent>` materialises one on demand.

## Membership

```bash
ngit repo accept --json                       # accept an invitation; add --grasp-server <url> to host the git data there too
ngit repo leave --json                        # end your own role and republish
ngit repo follow-lead --json                  # retain history and follow the resolved lead
```
