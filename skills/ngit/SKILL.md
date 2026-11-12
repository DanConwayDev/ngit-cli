---
name: ngit
description:
  Git collaboration over Nostr and Grasp (NIP-34) with the ngit CLI and
  git-remote-nostr. Use in any repository with a nostr:// remote, often called
  a Grasp repository, for collaboration requests (open an Issue, create,
  update, review or merge a Pull Request (PR), comment, clone)
  and whenever a task involves nostr:// URLs, ngit commands, Grasp servers,
  gitworkshop.dev, Nostr CI, software releases and Zapstore publication, OCI
  container images, or nsite static sites. Routes to the installed `ngit --help`
  for each workflow.
license: CC-BY-SA-4.0
metadata:
  version: "1.30"
---

# ngit — git collaboration over Nostr and Grasp

A repository with a `nostr://` remote (check `git remote -v`) keeps its issues,
pull requests, reviews, CI results and releases on Nostr. Use `ngit` and `git`
for them; do not substitute `gh`, `glab` or a forge API. People often call
these Grasp repositories, after the Grasp git servers that host them.

## Find the workflow in the installed help

The installed binary documents its own workflows, so this skill does not.

1. Run `ngit --help`. It ends with a workflow map and the conventions shared by
   every command.
2. Before acting, read the help the map names for the task, for example
   `ngit pr --help`. It holds that workflow's rules and pitfalls.
3. Use `ngit <command> <subcommand> --help` for exact arguments.

Some workflows run through plain git. Pushing a branch whose name starts with
`pr/` opens or updates a pull request, so read `ngit pr --help` before that
push. Ordinary `git clone`, `fetch` and `pull`, and pushes of other branches,
need no help lookup.

Read only the help the task needs, once per session. It describes the installed
version exactly: prefer it to remembered syntax, and do not check
`ngit --version` as a routine step.

## When that fails

- **`ngit` is not installed.** Say so and offer to install it
  (https://ngit.dev/install) instead of falling back to another tool.
- **`ngit --help` has no workflow map.** The binary predates workflow help.
  Say so and offer to update it (`ngit update`, or https://ngit.dev/install).
  Until then use `ngit <command> --help` for exact flags.
