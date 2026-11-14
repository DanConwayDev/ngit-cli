{ pkgs, craneLib, src }:
let
  darwin = pkgs.stdenv.hostPlatform.isDarwin;
  targets = if darwin then [ "aarch64-apple-darwin" "x86_64-apple-darwin" ] else [
    "x86_64-unknown-linux-gnu" "aarch64-unknown-linux-gnu"
    "x86_64-unknown-linux-musl" "x86_64-pc-windows-msvc"
  ];
  rust = pkgs.rust-bin.stable.latest.default.override { inherit targets; };
  vendor = craneLib.vendorCargoDeps { src = craneLib.cleanCargoSource src; };
  opensslPin = builtins.fromJSON (builtins.readFile ../../../tools/release/openssl-source.json);
  opensslSource = pkgs.fetchurl { inherit (opensslPin) url sha256; };
  recoveryPin = builtins.fromJSON (builtins.readFile ../../../tools/release/recovery-cli.json);
  recoveryCli = pkgs.stdenvNoCC.mkDerivation {
    pname = "ngit-recovery-cli";
    inherit (recoveryPin) version;
    src = pkgs.fetchurl { inherit (recoveryPin) url sha256; name = "ngit-recovery.tar.gz"; };
    sourceRoot = ".";
    installPhase = ''
      mkdir -p "$out/bin"
      install -m755 ngit git-remote-nostr "$out/bin/"
    '';
  };
  base = with pkgs; [ python3 bash git coreutils cacert ];
  command = name: inputs: script: pkgs.writeShellApplication {
    inherit name;
    runtimeInputs = base ++ inputs;
    # These tiny scripts change with the checkout. Realize them locally rather
    # than probing the fleet cache for each new command wrapper.
    derivationArgs = { preferLocalBuild = true; allowSubstitutes = false; };
    runtimeEnv.SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    text = script;
  };
in {
  build = command "build-release-archive" ([ rust pkgs.stdenv.cc pkgs.perl pkgs.gnumake pkgs.pkg-config pkgs.cmake pkgs.ninja ]
    ++ (if darwin then [ pkgs.clang pkgs.darwin.cctools ] else [ pkgs.binutils-unwrapped pkgs.cargo-zigbuild pkgs.zig_0_16 pkgs.cargo-xwin pkgs.llvmPackages.clang-unwrapped pkgs.llvmPackages.lld pkgs.llvmPackages.llvm ])) ''
    export NGIT_ARCHIVE_VENDOR=${vendor}
    export NGIT_ARCHIVE_OPENSSL_SOURCE=${opensslSource}
    ${pkgs.lib.optionalString (!darwin) "export NGIT_ARCHIVE_LOADER=${pkgs.glibc}/lib/ld-linux-x86-64.so.2 CARGO_ZIGBUILD_ZIG_PATH=${pkgs.zig_0_16}/bin/zig"}
    ${builtins.readFile ../../../tools/release/archive-build.sh}
  '';
  check-publisher = command "check-release-publisher" [] ''
    python3 tools/release/check-publication.py
  '';
  rehearse = command "rehearse-release" [] ''
    python3 tools/release/rehearsal.py
  '';
  publish = command "publish-release-archives" [] ''
    python3 tools/release/publish-release.py
  '';
  mirror = command "mirror-release-archives" [] ''
    python3 tools/release/github-mirror.py
  '';
  update-gh-setup = command "update-gh-setup-ngit" [ pkgs.nodejs pkgs.curl pkgs.gnutar pkgs.gzip pkgs.gawk pkgs.gnugrep pkgs.findutils ] ''
    python3 tools/release/setup_ngit.py
  '';
  update-docs = command "update-ngit-docs" [ pkgs.nodejs ] ''
    python3 tools/release/update_docs.py
  '';
  post-release-verify = command "verify-published-release" [ recoveryCli ] ''
    python3 tools/release/post_release.py
  '';
  post-release-rehearsal = command "rehearse-post-release" [ recoveryCli pkgs.nodejs pkgs.curl pkgs.gnutar pkgs.gzip pkgs.gawk pkgs.gnugrep pkgs.findutils ] ''
    python3 tools/release/post_release.py
  '';
  post-release-apply = command "apply-post-release" [ recoveryCli pkgs.nodejs pkgs.curl pkgs.gnutar pkgs.gzip pkgs.gawk pkgs.gnugrep pkgs.findutils ] ''
    python3 tools/release/post_release.py --apply
  '';
  collect = command "collect-release-archives" [] ''
    python3 -m unittest discover -s tools/release
    python3 tools/release/archives.py collect "$NGIT_CI_INPUTS" "$NGIT_CI_OUTPUT_DIR/release"
  '';
}
