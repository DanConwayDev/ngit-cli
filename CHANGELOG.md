# Changelog

All notable changes to ngit will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.1] - Fix Account Creation on NixOS

### Fixed

- **NIP-46 bunker url privacy** tag bunker pubkey rather than user pubkey to communicate with bunker
- **Create account** show nsec for manually setting nostr.nsec git config when not able to set global git config

## [2.0.0] - Pull Request support

### Breaking Changes

- **SSH Key Authentication in nostr:// URLs**: The user field in nostr git URLs (e.g., `nym1@ssh/npub123/identifier`) is now treated as an SSH key file location rather than an SSH user. SSH key can be specified as a file within `~/.ssh` (e.g., `~/.ssh/nym1`) or as a full/relative path. Most git servers expect the SSH user to be 'git', so specifying a different SSH key is the idiomatic way to use different credentials.

### Added

- **Pull Requests Support**: Introduced complete PR functionality for large contributions that would be too big for relays as patches:

  - Generate PR events for oversized patches automatically
  - Support PR updates and PR as patch revision
  - List open/draft proposals on repo relays/servers as `pr/*` branches and all proposals as `refs/pr/*` and `refs/pr/pr-by-id/head`
  - Push PRs to custom clone URLs with auto-fork creation fallback
  - Add `--force-pr` and `--force-patch` flags for manual control
  - Full NIP-34 compliance with `merge-base` tags

- **NIP-22 Status Events Support**: Read and process NIP-22 style status events for proposals and PRs

- **ngit sync command**: New command to synchronize git servers with nostr state

  - Optional `--force` flag for forced synchronization eg deleting refs on non-GRASP servers
  - `--ref-name` parameter to limit sync to a single reference

- **ngit init improvements** (simple model for non-grasp servers):

  - Use user's grasp list for defaults instead of hardcoded options
  - List and allow selection/deselection of non-grasp servers
  - Check and fetch origin refs when missing locally
  - Publish state event and sync when existing origin matches tip

- Allow specifying non-default SSH key in `nostr://` address

### Fixed

- **Git server timeouts**: More robust timeout enforcement in both ngit binary and remote helper
- **Annotated and lightweight tags**: Proper handling and pushing of all tag types
- **nostr:// URLs with NIP-05**:
  - Fixed URLs with NIP-05 addresses without local part
  - Allow NIP-05 domain without `_@` prefix
- **Sync and fetch improvements**:
  - Don't fetch tags already available locally
  - Fetch refs missing locally before sync, fail gracefully
  - Include all valid nostr state (was incorrectly filtering)
- **Repository state**: Only use state and announcements from authorized maintainers
- **Status events**: Only use status events from author and maintainers
- **Grasp server detection**: Fix to ensure no SSH fallback when not needed
- **NIP compliance updates**:
  - Fix `t` tag: `revision-root` → `root-revision` (NIP-34)
  - Fix mention marker → `q` tag (NIP-10 update)
- **Error handling**: Capture more errors when updating refs
- Suppress warnings for poorly formatted proposals (only show to maintainers/author)

### Changed

- Updated to latest rust-nostr v0.43
- Updated gitworkshop.dev URL format (now uses nevent)
- Removed blossom from grasp server detection (removed from grasp spec)
- Print event description before publishing for clearer terminal UI

## [1.7.4] - 2025-07-16

### Fixed
- Apply nip46 breaking changes as remote signers remove nip04 support
- Apply relay connection timeout once, instead of per request batch
- Add git server timeouts
- Bump all dependencies

## [1.7.3] - 2025-06-20

### Changed
- Rename ngit-relay to grasp

### Fixed
- Always include HEAD in state event

## [1.7.2] - 2025-06-18

### Fixed
- Fix clone when HEAD isn't in nostr state event

## [1.7.1] - 2025-06-17

### Fixed
- Add support for `git://` clone urls

## [1.7.0] - 2025-06-03

### Added
- Quality-of-life features for ngit-relay users
  - Detect ngit-relays and only attempt using unauthenticated http protocols
  - Better sync and less errors as nostr is the only way to push
- Overhaul `ngit init`
  - Add simple / advanced mode
  - Add support for ngit-relays
  - Specify blossom servers
  - Sensible defaults
- Add resiliency - push to all maintainer's relays and git servers
- Require additional maintainers to publish announcements before pushing
- Allow users to specify fallback relays see `ngit --customize`
- Add show npub command

### Fixed
- Use newest state event found, rather than oldest
- More resilient builds for platforms and distros

## [1.6.3] - 2025-05-12

### Fixed
- Fallback to http protocol if ssh is unavailable

## [1.6.2] - 2025-05-06

### Added
- Add event description for remote signing process

### Fixed
- Fix custom ports use for git servers

### Changed
- Bump all dependencies to latest major versions

## [1.6.1] - 2025-04-02

### Changed
- Build binaries for more OSes

## [1.6.0] - 2024-12-20

### Added
- Overhaul and simplify login experience
- Add `account` api with `login`, `logout` and `export-keys` commands
- Add sign up feature targeted at users new to nostr
- Support nip05 addresses in nostr git urls (e.g., `nostr://dan@gitworkshop.dev/ngit`)
- Rework `ngit init` to make on-boarding more intuitive with simpler questions and more guidance
- Expand merge types that automatically update PR status when pushed

### Changed
- Don't create `maintainers.yaml` for new repos but continue to support it for existing projects
- Remove ngit `pull`, `push` and `fetch` api to nudge users to use native git commands with git plugin
- Bump dependencies (e.g., rust-nostr to v0.37)

### Fixed
- Fix `ngit account login` from outside of a git repository
- Add QR code border
- Make `ngit list` prompts more intuitive

## [1.5.3] - 2024-11-12

### Fixed
- Fix remote signing as nip46 update has breaking changes
- Auth to relays on requests
- Fix `pr/` branch name prefix issue
- Fix `ngit init` error when remote added before initiation
- Don't blast initiation events as munity blaster is no more
- When git-remote-nostr called directly show help instead of error

### Changed
- Bump rust-nostr to v0.36
- Replace sqlite with lmdb due to rust-nostr deprecation

## [1.5.2] - 2024-09-24

### Added
- Login via nip46 QR code
- Enable login directly in git plugin
- Add resilience to git plugin so that a poorly formatted PR will gracefully fail

## [1.5.1] - 2024-09-20

### Changed
- Git plugin reports on event broadcasting

## [1.5.0] - 2024-09-18

### Added
- New nostr url format that works better for MacOS users: `nostr://<*protocol>/<npub123>/<*relay-hint>/<identifier>` (*optional)
- Status updates during clone, push and fetch
- Intelligent protocol selection and fallback
  - Unless unusual protocol specified in clone url it will try in this order:
    - fetch: https unauth, ssh, https
    - push: ssh, https auth
  - Save successful protocol in git config so it is tried first next time
  - Enable override from nostr url (will only use this protocol)
- Enable building binaries via nix

### Changed
- Refactor into lib and bin structure
- Bump dependencies

## [1.4.6] - 2024-09-13

### Fixed
- Fix `ngit push` and `ngit pull` when on a pr branch not in the format `pr/<branch-name>(<8-chars-from-id>)`

## [1.4.5] - 2024-08-30

### Added
- When clone url is ssh use auth for `list` and `fetch` as they are required
- When clone url is ssh, fallback to https so read events don't always require auth

### Fixed
- Stop asking for git server credentials when pushing `pr/` branch
- Fix `no repo events at specified coordinates` error via rust-nostr v0.34.1 upgrade

## [1.4.4] - 2024-08-27

### Added
- Include git plugin in release zip

## [1.4.3] - 2024-08-27

### Fixed
- Fix clone using nostr url

## [1.4.2] - 2024-08-20

### Fixed
- Only maintainers can push normal branches / tags

## [1.4.1] - 2024-08-20

### Fixed
- Fix pushing tags in git-remote-nostr

## [1.4.0] - 2024-08-20

### Added
- Add git-remote-nostr binary

## [1.3.1] - 2024-07-25

### Fixed
- Fix(init): update maintainers.yaml if identifier or relays have changed

## [1.3.0] - 2024-07-24

### Added
- NIP-46 remote signing (from Amber, etc)
- `list` breaks down proposals by status
- Local cache in `.git` to enable viewing proposals offline and reuse by other git clients
- Introduced `fetch` to download recent proposals
- Improved repo selection and handling of multiple maintainers
- Unique branch names for proposals to prevent name conflicts
- Login to different npubs for different repositories
- Store login details in git config so they can be reused by other git clients ran locally
- Add NIP-31 alt tags to events
- Add euc marker per NIP-34 tweak

### Fixed
- Ensure repo events of all maintainers are tagged in proposals
- Stop filtering out very large patches

## [1.3-beta1] - 2024-07-05

### Added
- Beta release for testing

## [1.2.1] - 2024-05-14

### Fixed
- Fix ngit init support for multiple maintainers

## [1.2.0] - 2024-05-14

### Added
- `ngit send --in-reply-to` tag any nostr notes and npubs in proposals
- `ngit send` link to proposal on gitworkshop

### Changed
- Remove unreliable relay.f7z.io from default relay set

## [1.1.2] - 2024-04-16

### Added
- Improve relay timeout behaviour
- Improve reliability via dependency upgrade
- Build via nix in ci

### Fixed
- Various reliability improvements

## [1.1.1] - 2024-03-08

### Fixed
- Fix stack overflow bug when origin remote doesn't exist

## [1.1.0] - 2024-03-08

### Added
- ngit send - improve proposal commit

## [1.0.0] - 2024-02-29

### Changed
- Major version to indicate breaking changes, not stability

## [0.1.2] - 2024-01-31

### Added
- Early release improvements

## [0.1.1] - 2024-01-26

### Added
- Early release improvements

## [0.1.0] - 2024-01-23

### Added
- Initial minor release

## [0.0.2] - 2023-05-23

### Added
- Early development release

## [0.0.1] - 2023-05-21

### Added
- Initial release
