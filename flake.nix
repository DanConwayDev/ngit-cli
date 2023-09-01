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
            # stable to be introduced when the following issue is resolved
            # https://github.com/oxalica/rust-overlay/issues/136
            # rust-bin.stable.latest.default
            # nightly for rustfmt
            (
              rust-bin.selectLatestNightlyWith (toolchain: toolchain.default.override {
                extensions = [
                  "rust-src"
                  "rustfmt"
                  "clippy"
                ];
              })
            )
          ];

          buildInputs = [
            rust-analyzer
            gitlint
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
