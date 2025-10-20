# AGENTS.md

Documentation for AI agents and automated tools working with the ngit codebase.

## Project Overview

**ngit** is a nostr plugin for git that enables decentralized code collaboration using the nostr protocol.

- **Language**: Rust
- **Type**: Command-line tool with git integration
- **Architecture**: Two main binaries (`ngit` and `git-remote-nostr`) with shared library code
- **Key Dependencies**: nostr-sdk, git2, clap, tokio

### Core Concepts

1. **Nostr Integration**: Uses nostr for repository identification, discovery, state management, and collaboration (PRs, issues)
2. **Git Server Agnostic**: Still requires git servers for data storage, but they're interchangeable
3. **Decentralized**: Multiple relays and git servers can be used for redundancy
4. **URL Format**: `nostr://<npub|nip05-address>/<identifier>`

## Project Structure

```
ngit/
├── src/
│   ├── bin/
│   │   ├── git_remote_nostr/    # Git remote helper implementation
│   │   │   ├── capabilities.rs
│   │   │   ├── fetch.rs
│   │   │   ├── list.rs
│   │   │   ├── main.rs
│   │   │   └── push.rs
│   │   └── ngit/                # Main CLI tool
│   │       ├── main.rs
│   │       └── sub_commands/
│   └── lib/                     # Shared library code
│       ├── git/                 # Git operations
│       ├── login/               # User authentication
│       ├── client.rs            # Nostr client
│       ├── fetch.rs             # Fetch operations
│       ├── git_events.rs        # Git event handling
│       ├── list.rs              # List operations
│       ├── push.rs              # Push operations
│       ├── repo_ref.rs          # Repository references
│       ├── repo_state.rs        # Repository state
│       └── utils.rs             # Utilities
├── tests/                       # Integration tests
├── test_utils/                  # Test utilities and helpers
└── git_hooks/                   # Git hooks for development
```

## Key Files and Their Purposes

### Core Library (`src/lib/`)

- **`repo_ref.rs`**: Repository reference handling, URL parsing, grasp server detection
  - Functions: `is_grasp_server_in_list()`, `is_grasp_server_clone_url()`, `normalize_grasp_server_url()`
  - Critical for identifying and validating repository URLs
  
- **`repo_state.rs`**: Manages repository state (refs, commits, etc.)

- **`client.rs`**: Nostr client wrapper and relay communication

- **`git_events.rs`**: Handles git-related nostr events

- **`push.rs`**: Push operations to git servers and nostr relays

- **`fetch.rs`**: Fetch operations from git servers

### Binaries

- **`git-remote-nostr`**: Git remote helper that enables git to work with nostr:// URLs
- **`ngit`**: Main CLI tool for managing nostr repositories

## Development Guidelines

### Code Style

- Follow Rust standard formatting (`rustfmt.toml` is configured)
- Run `cargo fmt` before committing
- Run `cargo clippy` to catch common issues
- Use `anyhow::Result` for error handling
- Prefer explicit error messages with context

### Testing

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_name

# Run tests with output
cargo test -- --nocapture

# Run integration tests only
cargo test --test ngit_init
```

### Important Testing Notes

1. **URL Comparison**: When comparing git server URLs, be aware of normalization:
   - `is_grasp_server_in_list()`: Compares full URLs (including repo path)
   - `normalize_grasp_server_url()`: Extracts server part only (strips npub and path)

2. **Test Structure**: Integration tests are in `tests/`, unit tests are in module files

### Building

```bash
# Development build
cargo build

# Release build
cargo build --release

# Binaries will be in target/debug/ or target/release/
# - ngit
# - git-remote-nostr
```

## Common Tasks for Agents

### Adding New Features

1. **Identify the correct module**: 
   - Git operations → `src/lib/git/`
   - Nostr operations → `src/lib/client.rs`, `src/lib/git_events.rs`
   - CLI commands → `src/bin/ngit/sub_commands/`
   - Remote helper → `src/bin/git_remote_nostr/`

2. **Add tests**: Always add tests for new functionality

3. **Update error handling**: Use `anyhow::Context` for error messages

4. **Check dependencies**: Avoid adding unnecessary dependencies

### Debugging Issues

1. **Check test failures**: Run `cargo test` to identify failing tests

2. **Review error context**: Look at the full error chain with `.context()`

3. **Examine URL handling**: Many issues relate to URL parsing/normalization
   - Check `repo_ref.rs` for URL-related functions
   - Verify grasp server detection logic

4. **Trace nostr events**: Check event creation and publishing in `git_events.rs`

### Refactoring

1. **Maintain backward compatibility**: This is a CLI tool users depend on

2. **Update all call sites**: Use `cargo check` to find all references

3. **Run full test suite**: Ensure nothing breaks

4. **Check integration tests**: These test real-world scenarios

## Architecture Patterns

### Error Handling

```rust
use anyhow::{Context, Result};

fn example() -> Result<()> {
    something()
        .context("descriptive error message")?;
    Ok(())
}
```

### Async Operations

- Uses `tokio` runtime for async operations
- Nostr operations are async
- Git operations are mostly sync

### Configuration

- User config stored in platform-specific directories (via `directories` crate)
- Repository config in `.git/config` and `ngit.yaml`

## Important Invariants

### URL Handling

1. **Grasp Server URLs**: Must contain npub in path format: `/{npub}/`
2. **Nostr URLs**: Format: `nostr://<npub|nip05>/<identifier>`
3. **Normalization**: Different functions normalize differently:
   - Full URL comparison: Use direct string comparison with `trim_end_matches('/')`
   - Server identification: Use `normalize_grasp_server_url()`

### Git Integration

1. **Remote helper protocol**: Must follow git's remote helper protocol exactly
2. **Ref naming**: PRs use `pr/` prefix
3. **State sync**: Must keep git refs in sync with nostr events

## Dependencies to Be Aware Of

### Critical Dependencies

- **nostr**: Core nostr protocol implementation
- **git2**: Git operations via libgit2
- **tokio**: Async runtime
- **anyhow**: Error handling

### Optional Features

Check `Cargo.toml` for feature flags and optional dependencies.

## Testing Strategy

### Unit Tests

- Located in same file as code (`#[cfg(test)] mod tests`)
- Test individual functions in isolation
- Example: `is_grasp_server_in_list` tests in `repo_ref.rs`

### Integration Tests

- Located in `tests/` directory
- Test full workflows (init, push, fetch, etc.)
- Require test utilities from `test_utils/`

### Test Utilities

- Mock relay implementation
- Git repository setup helpers
- Located in `test_utils/`

## Common Pitfalls

### 1. URL Normalization

**Problem**: Using wrong normalization function leads to incorrect comparisons.

**Solution**: 
- For full URL comparison: Direct string comparison
- For server identification: Use `normalize_grasp_server_url()`

### 2. Async/Sync Boundaries

**Problem**: Mixing async and sync code incorrectly.

**Solution**: Use `tokio::runtime::Runtime` or proper async context.

### 3. Git State

**Problem**: Git state getting out of sync with nostr events.

**Solution**: Always update both git refs and publish nostr events atomically.

## Contribution Workflow

1. **Clone**: Use ngit itself to clone: `nostr://dan@gitworkshop.dev/relay.damus.io/ngit`
2. **Branch**: Create a branch with `pr/` prefix for pull requests
3. **Test**: Run `cargo test` and `cargo clippy`
4. **Commit**: Follow git commit message conventions (enforced by `git_hooks/commit-msg`)
5. **Push**: Push branch to submit PR via ngit

## Resources

- **Main Repository**: [gitworkshop.dev/dan@gitworkshop.dev/ngit](https://gitworkshop.dev/dan@gitworkshop.dev/ngit)
- **Homepage**: [gitworkshop.dev/ngit](https://gitworkshop.dev/ngit)
- **Source**: [codeberg.org/DanConwayDev/ngit-cli](https://codeberg.org/DanConwayDev/ngit-cli)
- **Nostr Protocol**: [github.com/nostr-protocol/nostr](https://github.com/nostr-protocol/nostr)

## Quick Reference

### Build Commands

```bash
cargo build                    # Development build
cargo build --release         # Release build
cargo test                    # Run tests
cargo clippy                  # Run linter
cargo fmt                     # Format code
```

### File Locations

```bash
src/lib/repo_ref.rs           # URL handling, grasp server detection
src/lib/client.rs             # Nostr client
src/lib/git_events.rs         # Git event handling
src/bin/ngit/main.rs          # Main CLI entry point
src/bin/git_remote_nostr/     # Git remote helper
tests/                        # Integration tests
```

### Key Functions

```rust
// URL handling
is_grasp_server_in_list(url, list)     // Check if URL in list (exact match)
is_grasp_server_clone_url(url)         // Check if URL is grasp server format
normalize_grasp_server_url(url)        // Extract server part from URL

// Repository operations
repo_ref.to_nostr_git_url()            // Convert to nostr URL
push_to_remote()                       // Push to git server
fetch_from_remote()                    // Fetch from git server
```

## Support and Questions

For questions about the codebase or contributions:
- Check existing issues on [gitworkshop.dev](https://gitworkshop.dev/dan@gitworkshop.dev/ngit)
- Review test files for usage examples
- Examine integration tests for workflow examples

---

**Last Updated**: 2025-10-20  
**Version**: 1.7.4  
**Maintainer**: DanConwayDev
