{ pkgs, craneLib, src }:
let
  darwin = pkgs.stdenv.hostPlatform.isDarwin;
  targets = if darwin then [ "aarch64-apple-darwin" "x86_64-apple-darwin" ] else [
    "x86_64-unknown-linux-gnu" "aarch64-unknown-linux-gnu"
    "x86_64-unknown-linux-musl" "x86_64-pc-windows-msvc"
  ];
  rust = pkgs.rust-bin.stable.latest.default.override { inherit targets; };
  vendor = craneLib.vendorCargoDeps { src = craneLib.cleanCargoSource src; };
  base = with pkgs; [ python3 bash git coreutils cacert ];
  command = name: inputs: script: pkgs.writeShellApplication {
    inherit name;
    runtimeInputs = base ++ inputs;
    runtimeEnv.SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    text = script;
  };
in {
  build = command "build-release-archive" ([ rust pkgs.stdenv.cc pkgs.perl pkgs.gnumake pkgs.pkg-config pkgs.cmake pkgs.ninja ]
    ++ (if darwin then [ pkgs.clang pkgs.darwin.cctools ] else [ pkgs.binutils-unwrapped pkgs.cargo-zigbuild pkgs.zig_0_16 pkgs.cargo-xwin pkgs.llvmPackages.clang-unwrapped pkgs.llvmPackages.lld pkgs.llvmPackages.llvm ])) ''
    export NGIT_ARCHIVE_VENDOR=${vendor}
    ${pkgs.lib.optionalString (!darwin) "export NGIT_ARCHIVE_LOADER=${pkgs.glibc}/lib/ld-linux-x86-64.so.2 CARGO_ZIGBUILD_ZIG_PATH=${pkgs.zig_0_16}/bin/zig"}
    ${builtins.readFile ../../../tools/release/archive-build.sh}
  '';
  mirror = command "mirror-release-archives" [] ''
    python3 tools/release/github-mirror.py
  '';
  collect = command "collect-release-archives" [] ''
    python3 -m unittest discover -s tools/release
    python3 tools/release/archives.py collect "$NGIT_CI_INPUTS" "$NGIT_CI_OUTPUT_DIR/release"
  '';
}
