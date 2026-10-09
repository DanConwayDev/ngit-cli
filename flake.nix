{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";

    # Disabled until a25396d8 is resolved in ngit's released libgit2 dependency.
    # Restore this input and add `buzz` to the outputs arguments when re-enabling
    # the packages and shell hook below. Keep the reviewed packaging pin.
    # buzz = {
    #   url = "github:mulatta/buzz.nix/fc51e7947203fc8974fcea71cdea148a59d6d0e7";
    # };
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        manifest = pkgs.lib.importTOML ./Cargo.toml;
        # Pin the v3.0.7 release.
        ngitGraspRevision = "59efc8259a51c7204cd837f67cb8ec14156d2956";
        # The pinned repository contains Gitlinks without .gitmodules entries.
        # Nix 2.34 and 2.35 disagree about whether their empty directories are
        # retained in a flake Git input, producing different NAR hashes for the
        # same commit. fetchgit gives us a stable tracked-file checkout instead.
        ngitGraspSource = pkgs.fetchgit {
          url = "https://gitnostr.com/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit-grasp.git";
          rev = ngitGraspRevision;
          hash = "sha256-RCVgBBm+5m+BG4EEL3QTl+9QWJDPzrHl/pvJYcABpHM=";
          fetchSubmodules = false;
        };
        ngit-grasp-pkg = pkgs.rustPlatform.buildRustPackage {
          pname = "ngit-grasp";
          version = "3.0.7";
          src = ngitGraspSource;
          NGIT_BUILD_REVISION = ngitGraspRevision;
          cargoLock.lockFile = "${ngitGraspSource}/Cargo.lock";
          cargoBuildFlags = [ "-p" "ngit-grasp" ];
          nativeBuildInputs = with pkgs; [ pkg-config git ];
          buildInputs = with pkgs; [ openssl ];
          # The upstream library tests require Git and other ambient state that
          # is unavailable in the build sandbox. The ngit integration suite
          # exercises the resulting binary instead.
          doCheck = false;
        };
        # Restore with the Buzz CI steps once a25396d8 is resolved.
        # buzz-test-packages = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
        #   buzz.packages.${system}.buzz-server-binaries
        #   pkgs.postgresql_17
        #   pkgs.redis
        #   pkgs.garage_2
        # ];
      in with pkgs; {
        devShells.default = mkShell {

          nativeBuildInputs = [
            # override rustfmt with nightly toolchain version to support unstable features
            # ideally this wouldn't be pinned to a specific nightly version but
            # selectLatestNightlyWith isn't support with mixed toolchains
            # https://github.com/oxalica/rust-overlay/issues/136
            (lib.hiPrio rust-bin.nightly."2026-08-27".rustfmt)
            # (rust-bin.stable.latest.override { extensions = [ "rust-analyzer" ]; })
            rust-bin.stable.latest.default
          ];

          buildInputs = [
            pkg-config # required by git2
            gitlint
            openssl
            dbus
            ngit-grasp-pkg
          ]; # Restore `++ buzz-test-packages` once a25396d8 is resolved.
          shellHook = ''
            # auto-install git hooks
            dot_git="$(git rev-parse --git-common-dir)"
            if [[ ! -d "$dot_git/hooks" ]]; then mkdir "$dot_git/hooks"; fi
            for hook in git_hooks/* ; do ln -sf "$(pwd)/$hook" "$dot_git/hooks/" ; done

            # For rust-analyzer 'hover' tooltips to work.
            export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc}

            # Point the test harness at the exact pinned ngit-grasp binary.
            export NGIT_GRASP_BIN=${ngit-grasp-pkg}/bin/ngit-grasp

          '';
          # Once a25396d8 is resolved, append this to shellHook:
          # + lib.optionalString stdenv.hostPlatform.isLinux ''
          #   export BUZZ_RELAY_BIN=${buzz.packages.${system}.buzz-server-binaries}/bin/buzz-relay
          # ''
        };
        apps.ci-checks = {
          type = "app";
          program = "${pkgs.writeShellApplication {
            name = "ngit-ci-checks";
            runtimeInputs = [ pkgs.nix pkgs.git ];
            text = ''
              exec nix develop --command ${pkgs.bash}/bin/bash ${./nix/ci-checks.sh}
            '';
          }}/bin/ngit-ci-checks";
        };
        apps.publish-crate = {
          type = "app";
          program = "${pkgs.writeShellApplication {
            name = "ngit-publish-crate";
            runtimeInputs = [ pkgs.nix pkgs.git ];
            text = ''
              exec nix develop --command ${pkgs.bash}/bin/bash ${./nix/publish-crate.sh}
            '';
          }}/bin/ngit-publish-crate";
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
            dbus
          ];
          nativeBuildInputs = [
            pkg-config # required by git2
            openssl
            dbus
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
