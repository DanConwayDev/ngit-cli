#!/usr/bin/env bash
set -euo pipefail
case "${NGIT_CI_JOB_ID:?}" in
  linux-x86_64) target=x86_64-unknown-linux-gnu.2.17 ;;
  linux-aarch64) target=aarch64-unknown-linux-gnu.2.17 ;;
  linux-musl) target=x86_64-unknown-linux-musl ;;
  macos) target=universal-apple-darwin ;;
  windows) target=x86_64-pc-windows-msvc ;;
  *) echo 'Unknown release archive job' >&2; exit 1 ;;
esac
export CARGO_BUILD_JOBS=2
export XWIN_ARCH=x86_64 XWIN_SDK_VERSION=10.0.26100
export XWIN_CACHE_DIR="$PWD/target/xwin-cache"
if [[ $target == universal-apple-darwin ]]; then
  # Use the guest's Apple SDK and linker for portable downloads, not Nix dylibs.
  mkdir -p "$PWD/target/apple-tools"
  ln -sf /usr/bin/clang "$PWD/target/apple-tools/clang"
  export PATH="$PWD/target/apple-tools:$PATH"
  export CC=/usr/bin/clang CXX=/usr/bin/clang++
  export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/clang
  export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=/usr/bin/clang
  export SDKROOT
  SDKROOT=$(/usr/bin/xcrun --sdk macosx --show-sdk-path)
fi
export CARGO_HOME="$PWD/target/archive-cargo-home"
export CARGO_TARGET_DIR="$PWD/target/archives"
mkdir -p "$CARGO_HOME" "$CARGO_TARGET_DIR"
cp --remove-destination "$NGIT_ARCHIVE_VENDOR/config.toml" "$CARGO_HOME/config.toml"
# Do not inherit the Nix development shell's host library/compiler choices.
unset OPENSSL_DIR OPENSSL_LIB_DIR OPENSSL_INCLUDE_DIR PKG_CONFIG_PATH
unset NIX_CFLAGS_COMPILE NIX_LDFLAGS
unset LIBSSH2_SYS_USE_PKG_CONFIG
export LIBGIT2_NO_PKG_CONFIG=1
if [[ $target != *windows* ]]; then
  env_file="$CARGO_TARGET_DIR/openssl.env"
  python3 tools/release/build-openssl.py "$target" --output "$CARGO_TARGET_DIR/openssl" --env-file "$env_file" --jobs 2
  while IFS='=' read -r key value; do export "$key=$value"; done < "$env_file"
fi
build_args=(--release --locked --offline --bins --features vendored-openssl)
if [[ $target == universal-apple-darwin ]]; then
  for arch in aarch64 x86_64; do
    export MACOSX_DEPLOYMENT_TARGET=11.0
    [[ $arch != x86_64 ]] || export MACOSX_DEPLOYMENT_TARGET=10.13
    cargo build "${build_args[@]}" --target "$arch-apple-darwin"
  done
  mkdir -p "$CARGO_TARGET_DIR/$target/release"
  for binary in ngit git-remote-nostr; do
    lipo -create "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/$binary" \
      "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/$binary" \
      -output "$CARGO_TARGET_DIR/$target/release/$binary"
    /usr/bin/codesign --force --sign - "$CARGO_TARGET_DIR/$target/release/$binary"
  done
elif [[ $target == *windows* ]]; then
  cargo xwin build "${build_args[@]}" --target "$target"
else
  cargo zigbuild "${build_args[@]}" --target "$target"
fi
python3 tools/release/archives.py pack "$target" "$CARGO_TARGET_DIR/${target%.2.17}/release" "${NGIT_CI_OUTPUT_DIR:?}"
