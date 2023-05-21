# ngit cli

a proof of concept cli for a nostr based github alternative.

This cli is part of a wider 'Code Collaboration over Nostr' project to establish the use of nostr for code collaboration.

Warning: This project is in an alpha stage so breaking changes in quick succession are very likely. This is proof of concept, expect things not to work. Do not use for production purposes. It currently doesn't encrypt private keys. Use with caution.

## Getting Started

1. install binary
1. browse and clone
    1. browse repositories with ```ngit clone```
    1. clone hello-world repository with:

        ```
        ngit clone -r nevent1qqspg8u2a5ql5w739fyfzq43c2k29h8znpv0xads3njucs63wsd7wyspz3mhxue69uhhyetvv9ujuerpd46hxtnfduu6qm7j
        ```
    1. clone ngit-cli repository with:

        ```
        ngit clone -r nevent1qqsql3pzeypfr9e0c3fpnnfm3u5v5h37kmu2q2pxr3mspcuhqy6f78cpz3mhxue69uhhyetvv9ujuerpd46hxtnfduq3qamnwvaz7tmwdaehgu3wwa5kuegm87gy2
        ```
    1. ```ngit prs``` to browse open pull requests. Pull to a local branch to see the changes.
1. create repository with ```ngit init```
1. pull, push, pull request and merge
    1. make a branch off main / master.
    1. commit with standard ```git``` command
    1. publish branch and raise a PR with ```ngit push```
    1. pull branch updates with ```ngit pull```
    1. merge pr with ```ngit merge``` when on the selected branch

## Concept Overview

This cli replaces all interaction that would traditionally be done with a remote git server, with a nostr based alternative centered around commits as patches.

Commits, branches, pull requests and merges are all nostr events. Permissions are managed through groups, so multiple maintainers can manage a repository.

Forks are replaced by permissioned branches. This makes it easy for contributors to create PRs and for maintainers and others to review them.

This model reduces the barriers for clients to support repository collaboration. They do not need interact with a git server, clone a repository or download many events. Simply display the `kind 1` messages related to a PR or an issue. The collaboration experience can be enriched further by opting-in to features such as code comments, merging and permissions validation.

Large patches and binaries would need a to be transported separately and referenced in a stub patch event.

### Current Supported Commands
```ngit init```
* creates a maintainers group event `kind: 40000` (see draft NIP)
    * creates an admin group event (or uses default) 
* creates a repo event `kind: 420`
* scaffolds a structure within `.ngit` folder and adds it to `.gitignore`
* initializes git rep
* broadcasts events

```ngit push```
* looks for a mapping between local branch and remote
    * if it finds a mapping:
        * fetches patches and stops if local branch is 'behind' remote
    * if it doesn't find a mapping:
        * creates a branch maintainers group with the user and the repo maintainers group as members (skips this if the user a repo maintainer)
        * creates branch event branch `kind: 410`
* generates one patch `kind: 410` per new commit and broadcasts it
* TODO: enable ```--force``` so that branches can be rebased. pull needs to be updated to handle this.

```ngit pull```
* pulls new patches (or patches that have been merged) for the current branch
* check they were issued or merged by a branch maintainer and applies them
* pull new remote branch into a new local branch with option ```--branch [hex, nevent or note]``` 

```ngit prs```
* lists unmerged pull requests in current repository.
* views pull request details and allows users to pull the related branch

```ngit merge```
* checks user has permission to merge
* merges the current branch into main
* broadcasts a merge event
* creates a merge event which, when applied through ```ngit pull`` will fast-forward commits onto the target branch
* currently only supports merging to the main branch

```ngit clone```
* lists a selection of repositories on connected relays
* select and clone

```ngit clone -r [hex, nevent or note]```
* clones an ngit repository 

```ngit fetch```
* fetches patches and merges on checkedout branch from relays and reports on the number of commits 'behind' (ie. to pull)
* compares the local checkedout branch to patches that have been pushed and reports on number of commits 'ahead' (ie. to push)

```ngit rebroadcast```
* rebroadcast all locally stored events for a repository to selected relays

```ngit change-user```
* replace the stored private keys with new ones
