#!/usr/bin/env bash
set -euo pipefail

# Reject a branch/manual invocation before reading credentials.
[[ ${NGIT_CI_TRIGGER:-} == push && ${NGIT_CI_REF_TYPE:-} == tag ]] || {
  echo "Crate publication requires a tag push" >&2
  exit 1
}
version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "ngit") | .version')
[[ ${NGIT_CI_REF:-} == "refs/tags/v$version" ]] || {
  echo "Release tag must match Cargo.toml version v$version" >&2
  exit 1
}
CARGO_REGISTRY_TOKEN=$(cat "${NGIT_CI_SECRETS_DIR:?}/CARGO_REGISTRY_TOKEN")
: "${CARGO_REGISTRY_TOKEN:?The repository CARGO_REGISTRY_TOKEN secret is empty}"
export CARGO_REGISTRY_TOKEN
if [[ -n ${NGIT_CI_CANCEL_FILE:-} && -e $NGIT_CI_CANCEL_FILE ]]; then
  echo 'Cancelled before publication began' >&2
  exit 143
fi
# TERM reaches this command; let an active registry publication finish within
# the workflow grace period. Forced cancellation remains provider-controlled.
interrupted=0
trap 'interrupted=1; echo "Shutdown requested; finishing crate publication" >&2' TERM
cargo publish --locked --registry crates-io --package ngit &
pid=$!
while :; do
  interrupted=0
  if wait "$pid"; then status=0; else status=$?; fi
  (( interrupted )) || break
done
exit "$status"
