{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in
      with pkgs;
      {
        devShells.default = mkShell {

          nativeBuildInputs = [
            # override rustfmt with nightly toolchain version to support unstable features
            # ideally this wouldn't be pinned to a specific nightly version but
            # selectLatestNightlyWith isn't support with mixed toolchains
            # https://github.com/oxalica/rust-overlay/issues/136
            (lib.hiPrio rust-bin.nightly."2024-04-05".rustfmt)
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
      }
    );
}
