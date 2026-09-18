{ withEmulator ? true }:
let
  lock = builtins.fromJSON (builtins.readFile ../../flake.lock);
  nixpkgs = builtins.fetchTree lock.nodes.nixpkgs.locked;
  rustOverlay = builtins.fetchTree lock.nodes.rust-overlay.locked;
  pkgs = import nixpkgs {
    system = builtins.currentSystem;
    overlays = [ (import rustOverlay) ];
    config = { allowUnfree = true; android_sdk.accept_license = true; };
  };
  android = pkgs.androidenv.composeAndroidPackages {
    platformVersions = [ "35" ];
    buildToolsVersions = [ "35.0.0" ];
    includeEmulator = withEmulator;
    includeSystemImages = withEmulator;
    systemImageTypes = [ "default" ];
    abiVersions = [ "x86_64" ];
    includeNDK = true;
    ndkVersions = [ "27.2.12479018" ];
  };
in
assert pkgs.lib.assertMsg (pkgs.stdenv.hostPlatform.system == "x86_64-linux")
  "The Android smoke-test shell requires x86_64 Linux.";
pkgs.mkShell {
  packages = [ android.androidsdk pkgs.jdk17 pkgs.python3 pkgs.openssl pkgs.pkg-config pkgs.perl pkgs.gnumake
    (pkgs.rust-bin.stable.latest.default.override { targets = [ "x86_64-linux-android" ]; }) ];
  ANDROID_SDK_ROOT = "${android.androidsdk}/libexec/android-sdk";
  shellHook = ''
    export ANDROID_NDK_ROOT="$ANDROID_SDK_ROOT/ndk/27.2.12479018"
    export PATH="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/bin:$PATH"
    export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER=x86_64-linux-android28-clang
    export CC_x86_64_linux_android=x86_64-linux-android28-clang
    export AR_x86_64_linux_android=llvm-ar
  '';
}
