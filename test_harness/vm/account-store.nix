# Build with the repository's pinned nixpkgs; workspace is the absolute path
# of a checkout containing already-built account integration test binaries.
{ pkgs, workspace }:
let
  vm = import (pkgs.path + "/nixos") {
    configuration = {
      imports = [ (pkgs.path + "/nixos/modules/virtualisation/qemu-vm.nix") ];
      nixpkgs.pkgs = pkgs;
      system.stateVersion = "26.05";
      networking.hostName = "ngit-account-store-test";
      virtualisation = {
        memorySize = 2048;
        cores = 2;
        graphics = false;
        diskSize = 1024;
        diskImage = "${workspace}/target/account-store.qcow2";
        sharedDirectories.audit = {
          source = workspace;
          target = workspace;
        };
      };
      # This disposable guest contains synthetic test identities only. Run the
      # keyring as a normal user, avoiding root's capability-dropping behavior.
      users.users.audit.isNormalUser = true;
      services.getty.autologinUser = "root";
      services.dbus.enable = true;
      services.gnome.gnome-keyring.enable = true;
      environment.systemPackages = with pkgs; [ git dbus gnome-keyring libsecret python3 ];
    };
  };
in
vm.config.system.build.vm
