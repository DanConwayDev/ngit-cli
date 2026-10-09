#!/usr/bin/env bash
set -euo pipefail

run_step() {
  local id="$1" label="$2"
  shift 2
  if [[ -n ${NGIT_CI_STEP:-} ]]; then
    "$NGIT_CI_STEP" "$id" "$label" "$@"
  else
    "$@"
  fi
}

export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
run_step lint "Lint Rust" cargo clippy --locked
run_step format "Check formatting" cargo fmt --all -- --check
run_step compile-tests "Compile test binaries" cargo test --workspace --locked --no-run
run_step tests "Run tests" cargo test --workspace --locked
# Re-enable the pinned Buzz test once a25396d8 is resolved.
