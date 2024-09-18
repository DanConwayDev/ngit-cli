{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        manifest = pkgs.lib.importTOML ./Cargo.toml;
      in
      with pkgs;
      {
        devShells.default = mkShell {

          nativeBuildInputs = [
            # override rustfmt with nightly toolchain version to support unstable features
            # ideally this wouldn't be pinned to a specific nightly version but
            # selectLatestNightlyWith isn't support with mixed toolchains
            # https://github.com/oxalica/rust-overlay/issues/136
            (lib.hiPrio rust-bin.nightly."2024-09-17".rustfmt)
            # (rust-bin.stable.latest.override { extensions = [ "rust-analyzer" ]; })
            rust-bin.stable.latest.default
          ];

          buildInputs = [
            pkg-config # required by git2
            gitlint
            openssl
          ];
          shellHook = ''
            # auto-install git hooks
            dot_git="$(git rev-parse --git-common-dir)"
            if [[ ! -d "$dot_git/hooks" ]]; then mkdir "$dot_git/hooks"; fi
            for hook in git_hooks/* ; do ln -sf "$(pwd)/$hook" "$dot_git/hooks/" ; done

            # For rust-analyzer 'hover' tooltips to work.
            export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc}
          '';
        };
        # Create packages for each binary defined in Cargo.toml
        packages.default = pkgs.rustPlatform.buildRustPackage {
            pname = manifest.package.name;
            version = manifest.package.version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes = {
                "rexpect-0.5.0" = "0amxyp81r90gfqlx5dnfjsmd403kf5hcw0crzpcmsbaviavxff4y";
                "simple-websockets-0.1.6" = "0910bbl7p3by18w3wks8gbgdg8879hn2c39z1bkr5pcvfkcxmaf3";
              };
            };
            buildInputs = [
              pkg-config # required by git2
              openssl
            ];
            nativeBuildInputs = [
              pkg-config # required by git2
              openssl
            ];
          };
        # Create a tarball for the built package
        packages.tarball = stdenv.mkDerivation {
          name = "${manifest.package.name}-${manifest.package.version}-${system}.tar.gz";
          buildInputs = [ coreutils ];
          buildPhase = ''
            tar -czf $out/${manifest.package.name}-${manifest.package.version}-${system}.tar.gz -C $out .
          '';
        };
      }
    );
}