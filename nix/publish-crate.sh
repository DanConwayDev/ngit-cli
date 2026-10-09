#!/usr/bin/env bash
set -euo pipefail

# Reject a branch/manual invocation before reading credentials.
[[ ${NGIT_CI_TRIGGER:-} == push && ${NGIT_CI_REF_TYPE:-} == tag ]] || {
  echo "Crate publication requires a tag push" >&2
  exit 1
}
version=$(nix eval --impure --raw --expr '(builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version')
[[ ${NGIT_CI_REF:-} == "refs/tags/v$version" ]] || {
  echo "Release tag must match Cargo.toml version v$version" >&2
  exit 1
}
CARGO_REGISTRY_TOKEN=$(cat "${NGIT_CI_SECRETS_DIR:?}/CARGO_REGISTRY_TOKEN")
: "${CARGO_REGISTRY_TOKEN:?The repository CARGO_REGISTRY_TOKEN secret is empty}"
export CARGO_REGISTRY_TOKEN
exec cargo publish --locked --registry crates-io --package ngit
