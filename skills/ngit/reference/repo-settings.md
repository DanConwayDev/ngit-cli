# Repository settings and membership

Part of the ngit skill. Read this before changing a repository announcement,
its hosting, or its maintainer and moderator roster.

## Inspect before editing

```bash
ngit repo --json --offline
```

Repository announcements contain effective `git_servers`, `relays`, and
`hashtags`, plus `grasp_servers` detected from paired clone and relay entries.
Run `git fetch origin` first when the cache may be stale.

## Initial settings

`ngit init` declares the initial complete announcement. Grasp is the normal
hosting path and provides both a Git server and a Nostr relay:

```bash
ngit init --name "My Project" --grasp-server grasp.example.com --defaults --json
```

Without `--grasp-server`, `--defaults` uses the account's preferred grasp
servers and falls back to ngit's defaults. Additional infrastructure is
explicit, is empty by default, and supplements the grasp hosting rather than
replacing it:

```bash
ngit init \
  --name "My Project" \
  --additional-relay wss://relay.example.com \
  --additional-clone https://git.example.com/my-project.git \
  --defaults \
  --json
```

Hosting a repository without any grasp server has to be stated explicitly with
an empty `--grasp-server` value. It then needs both an additional relay and an
additional clone URL of its own:

```bash
ngit init \
  --name "My Project" \
  --grasp-server "" \
  --additional-relay wss://relay.example.com \
  --additional-clone https://git.example.com/my-project.git \
  --defaults \
  --json
```

`--identifier` is available during initial publication. It is not editable:
changing the NIP-34 `d` tag creates a different repository coordinate.

## Targeted edits

`ngit repo edit` preserves omitted settings. Collection settings use targeted,
repeatable actions rather than replacing the complete list:

| Setting | Add | Remove |
| ------- | --- | ------ |
| Grasp server | `--add-grasp-server URL` | `--remove-grasp-server URL` |
| Additional relay | `--add-additional-relay URL` | `--remove-additional-relay URL` |
| Additional clone | `--add-additional-clone URL` | `--remove-additional-clone URL` |
| Hashtag | `--add-hashtag TAG` | `--remove-hashtag TAG` |

The add and remove options may be repeated and combined in one command:

```bash
ngit repo edit \
  --remove-additional-relay wss://old.example.com \
  --add-additional-relay wss://new.example.com \
  --add-hashtag rust \
  --json
```

To empty a collection, repeat its remove action for every value currently
reported by `ngit repo --json --offline`.

Scalar and structured metadata retain direct replacement flags such as
`--name`, `--description`, `--web`, `--u`, and
`--earliest-unique-commit`.

Effective infrastructure is:

```text
relays = grasp-derived relays + additional relays
clones = grasp-derived clones + additional clones
```

A grasp-derived entry cannot be removed as an additional entry. Remove its
grasp server instead; ngit then removes the paired relay and clone together.
An edit that would leave no announcement relay or no Git server fails before
publication.

Every successful edit publishes a fresh announcement. When the repository has
Nostr state, ngit also republishes that state once, giving newly added relays
and Git servers the authoritative refs immediately. If publication cannot
establish the fresh state, follow the reported `ngit sync` recovery guidance.

## Maintainer model highlights

- A **co-maintainer** may publish Git state, merge proposals, manage issues and
  proposals, and add or remove members at the protocol level.
- A **lead maintainer** has the same authority and additionally takes
  responsibility for coordinating the roster. When a lead is present, clients
  expect co-maintainer coordinates to forward to that lead and ngit restricts
  normal roster management to the lead-shaped workflow.
- A **moderator** may publish issue, proposal, and patch status events,
  including recording an existing merge, but cannot publish Git state or
  perform a Git merge.

Membership is reciprocal. A listing is an invitation until its subject
publishes an announcement acknowledging the role. The normal workflow is:

```bash
# Alice invites Bob; Alice becomes the lead automatically.
ngit repo edit --add-maintainer <bob-npub> --json

# Bob accepts and confirms Alice as lead.
ngit repo accept --json

# Members retain history and follow a changed lead or roster.
ngit repo follow-lead --json
```

Change one maintainer relationship at a time:

```bash
ngit repo edit --remove-maintainer <npub> --json
ngit repo edit --lead-maintainer <npub> --json
ngit repo edit --acknowledge-maintainer-change <npub> --json
```

The first add by a sole maintainer establishes that publisher as lead. In the
deliberately leadless edge case, pass `--no-lead-maintainer` with every
`--add-maintainer` or `--remove-maintainer`. A lead handover requires the
proposed lead to publish the complete current roster before the old lead points
forward.

Inspect `members`, `lead_source`, `lead_path`, `pending_actions`, and `health`
in `ngit repo --json --offline` before repairing an unusual topology. Follow
the actionable error rather than replacing an announcement wholesale.
