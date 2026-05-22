{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";

    # ngit-grasp provides the GRASP server binary used by the integration
    # test harness. Pinned to a specific rev so CI is reproducible — bump
    # it intentionally rather than tracking a moving target.
    ngit-grasp = {
      url = "git+https://gitnostr.com/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit-grasp.git";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ngit-grasp, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        manifest = pkgs.lib.importTOML ./Cargo.toml;
        # ngit-grasp's upstream derivation runs cargo test --lib during the
        # nix build; 15 of those tests fail inside the build sandbox (they
        # need git in PATH or other ambient state). We only want the
        # binary, so disable the test phase here.
        ngit-grasp-pkg =
          ngit-grasp.packages.${system}.default.overrideAttrs (_: {
            doCheck = false;
          });
      in with pkgs; {
        devShells.default = mkShell {

          nativeBuildInputs = [
            # override rustfmt with nightly toolchain version to support unstable features
            # ideally this wouldn't be pinned to a specific nightly version but
            # selectLatestNightlyWith isn't support with mixed toolchains
            # https://github.com/oxalica/rust-overlay/issues/136
            (lib.hiPrio rust-bin.nightly."2025-10-16".rustfmt)
            # (rust-bin.stable.latest.override { extensions = [ "rust-analyzer" ]; })
            rust-bin.stable.latest.default
          ];

          buildInputs = [
            pkg-config # required by git2
            gitlint
            openssl
            ngit-grasp-pkg
          ];
          shellHook = ''
            # auto-install git hooks
            dot_git="$(git rev-parse --git-common-dir)"
            if [[ ! -d "$dot_git/hooks" ]]; then mkdir "$dot_git/hooks"; fi
            for hook in git_hooks/* ; do ln -sf "$(pwd)/$hook" "$dot_git/hooks/" ; done

            # For rust-analyzer 'hover' tooltips to work.
            export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc}

            # Point the test harness at the pinned ngit-grasp binary from
            # the flake input. Without this the harness falls back to the
            # sibling-clone heuristic, which is fine for local dev but
            # not what CI gets.
            export NGIT_GRASP_BIN=${ngit-grasp-pkg}/bin/ngit-grasp
          '';
        };
        # Create packages for each binary defined in Cargo.toml
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.package.name;
          version = manifest.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildInputs = [
            pkg-config # required by git2
            openssl
          ];
          nativeBuildInputs = [
            pkg-config # required by git2
            openssl
          ];
          doCheck = false;
        };
        # Create a tarball for the built package
        packages.tarball = stdenv.mkDerivation {
          name =
            "${manifest.package.name}-${manifest.package.version}-${system}.tar.gz";
          buildInputs = [ coreutils ];
          buildPhase = ''
            tar -czf $out/${manifest.package.name}-${manifest.package.version}-${system}.tar.gz -C $out .
          '';
        };
      });
}
