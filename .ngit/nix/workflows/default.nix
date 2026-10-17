{ pkgs, craneLib, src, ngit-grasp }:
let
  cargo = {
    src = craneLib.cleanCargoSource src;
    strictDeps = true;
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [ pkgs.openssl pkgs.dbus ];
  };
  dependencies = craneLib.buildDepsOnly (cargo // {
    pname = "ngit-test-dependencies";
    version = "0.0.0";
    CARGO_PROFILE = "dev";
    CARGO_INCREMENTAL = "0";
    CARGO_PROFILE_DEV_DEBUG = "0";
    cargoExtraArgs = "--workspace --locked";
    installCargoArtifactsMode = "use-zstd";
    doCompressAndInstallFullArchive = true;
  });
  vendor = craneLib.vendorCargoDeps { inherit (cargo) src; };
  tools = with pkgs; [
      rust-bin.stable.latest.default stdenv.cc pkg-config
      bash git coreutils jq gnumake gnused gnugrep gnutar gzip gawk findutils
  ];
  command = name: script: pkgs.writeShellApplication {
    inherit name;
    runtimeInputs = tools;
    runtimeEnv = {
      PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" [ pkgs.openssl.dev pkgs.dbus.dev ];
      SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    };
    text = script;
  };
  rust = name: script: command name ''
    # Match the cached dev-profile objects; keep current source and tests local.
    export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
    export CARGO_TARGET_DIR="''${CARGO_TARGET_DIR:-$PWD/target}"
    if [[ ! -e "$CARGO_TARGET_DIR/.rustc_info.json" ]]; then
      mkdir -p "$CARGO_TARGET_DIR"
      ${pkgs.gnutar}/bin/tar --use-compress-program=${pkgs.zstd}/bin/zstd \
        -xf ${dependencies}/target.tar.zst -C "$CARGO_TARGET_DIR"
    fi
    export CARGO_HOME="$CARGO_TARGET_DIR/ci-cargo-home"
    mkdir -p "$CARGO_HOME"
    cp --remove-destination ${vendor}/config.toml "$CARGO_HOME/config.toml"
    ${script}
  '';
in {
  # Compressed Cargo objects do not retain references to all their build inputs.
  dependencies = pkgs.linkFarm "ngit-ci-dependencies" ([
    { name = "objects"; path = dependencies; }
    { name = "sources"; path = vendor; }
    { name = "rustfmt"; path = pkgs.rust-bin.nightly."2026-08-27".rustfmt; }
    { name = "openssl"; path = pkgs.openssl.dev; }
    { name = "dbus"; path = pkgs.dbus.dev; }
  ] ++ map (tool: { name = tool.name; path = tool; }) tools);
  test-server = ngit-grasp;
  commands = {
    ci.rust = {
      format = command "format" ''
        export PATH=${pkgs.rust-bin.nightly."2026-08-27".rustfmt}/bin:$PATH
        cargo fmt --all -- --check
      '';
      compile = rust "compile" "cargo test --workspace --locked --offline --no-run";
      lint = rust "lint" "cargo clippy --workspace --all-targets --locked --offline -- --deny warnings";
      tests = rust "tests" ''
        export NGIT_GRASP_BIN=${ngit-grasp}/bin/ngit-grasp
        cargo test --workspace --locked --offline
      '';
    };
    release.publish = command "publish" (builtins.readFile ./publish-crate.sh);
  };
}
