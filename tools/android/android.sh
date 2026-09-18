#!/usr/bin/env bash
# Manual smoke tests only; no CI or device installation is performed.
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
output_dir="${NGIT_ANDROID_TEST_DIR:-$repo_root/target/android-smoke}"
mkdir -p "$output_dir"
output_dir="$(cd -- "$output_dir" && pwd)"
# Keep emulator state local to this run rather than the user's regular AVDs.
export ANDROID_USER_HOME="$output_dir/state"
export ANDROID_AVD_HOME="$ANDROID_USER_HOME/avd"
serial=emulator-5580

case "${1:-}" in
  build)
    export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
    export CARGO_PROFILE_DEV_DEBUG=0
    export CARGO_PROFILE_TEST_DEBUG=0
    export CARGO_TARGET_DIR="$output_dir/cargo"
    cargo test --locked --target x86_64-linux-android --features vendored-openssl,android-smoke \
      --example android_tls --no-run --message-format=json > "$output_dir/test-artifacts.json"
    python3 - "$output_dir" <<'PY'
import json
from pathlib import Path
import shutil
import sys

output = Path(sys.argv[1])
for line in (output / "test-artifacts.json").read_text().splitlines():
    record = json.loads(line)
    if (record.get("reason") == "compiler-artifact"
            and record.get("target", {}).get("name") == "android_tls"
            and record.get("profile", {}).get("test")
            and record.get("executable")):
        shutil.copy2(record["executable"], output / "tls-tests")
        break
else:
    raise SystemExit("Cargo did not produce the Android TLS test executable")
PY
    cargo build --locked --target x86_64-linux-android --features vendored-openssl,android-smoke \
      --bins --example android_tls
    cp "$CARGO_TARGET_DIR/x86_64-linux-android/debug/ngit" "$output_dir/ngit"
    cp "$CARGO_TARGET_DIR/x86_64-linux-android/debug/git-remote-nostr" "$output_dir/git-remote-nostr"
    cp "$CARGO_TARGET_DIR/x86_64-linux-android/debug/examples/android_tls" "$output_dir/probe"
    ;;
  start)
    export ANDROID_HOME="${ANDROID_SDK_ROOT:?Enter tools/android/shell.nix first}"
    mkdir -p "$ANDROID_AVD_HOME"
    if [[ ! -f "$ANDROID_AVD_HOME/ngit-tls.ini" ]]; then
      printf 'no\n' | avdmanager create avd --name ngit-tls \
        --package 'system-images;android-35;default;x86_64'
    fi
    adb start-server
    exec emulator -avd ngit-tls -no-metrics -no-window -no-audio -no-boot-anim \
      -no-snapshot -gpu swiftshader -memory 2048 -cores 2 -port 5580
    ;;
  test)
    if [[ $# -gt 2 || (${2:-} != '' && ${2:-} != --online) ]]; then
      echo 'usage: tools/android/android.sh test [--online]' >&2
      exit 2
    fi
    for binary in tls-tests ngit probe; do
      [[ -x "$output_dir/$binary" ]] || { echo 'Run the build command first' >&2; exit 1; }
    done
    adb start-server
    python3 tools/android/wait-for-boot.py "$serial"
    adb -s "$serial" shell mkdir -p /data/local/tmp/ngit-smoke
    for binary in tls-tests ngit probe; do
      adb -s "$serial" push "$output_dir/$binary" "/data/local/tmp/ngit-smoke/$binary"
      adb -s "$serial" shell chmod 755 "/data/local/tmp/ngit-smoke/$binary"
    done
    timeout 60 adb -s "$serial" shell /data/local/tmp/ngit-smoke/tls-tests --nocapture
    timeout 15 adb -s "$serial" shell /data/local/tmp/ngit-smoke/ngit --help >/dev/null
    if [[ ${2:-} == --online ]]; then
      timeout 30 adb -s "$serial" shell /data/local/tmp/ngit-smoke/probe https://example.com
    fi
    ;;
  stop)
    adb -s "$serial" emu kill
    ;;
  *)
    echo 'usage: tools/android/android.sh {build|start|test [--online]|stop}' >&2
    exit 2
    ;;
esac
