# Changelog

All notable changes to ngit will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

## [1.7.4] - Previous Release
