{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    cattix.url = "path:../../..";
  };

  outputs = { nixpkgs, cattix, ... }:
    let
      system = "x86_64-linux";
      mkHost = name: order: nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          cattix.nixosModules.default
          cattix.nixosModules.haproxy
          ({ ... }: {
            system.stateVersion = "24.11";
            system.configurationRevision = "fixture-revision";
            networking.hostName = name;
            fileSystems."/" = {
              device = "/dev/vda";
              fsType = "ext4";
            };
            boot.loader.grub.devices = [ "/dev/vda" ];
            cattix = {
              group = "app";
              inherit order;
              target = {
                host = "127.0.0.1";
                port = 2222 + order;
                user = "root";
              };
              metadata = {
                environment = "test";
                criticality = "low";
                owner = "tests";
              };
              extensions.haproxy = {
                sockets = [ "/run/haproxy/admin.sock" ];
                backend = "be_app";
                server = name;
              };
              healthChecks = [
                {
                  name = "units";
                  command = {
                    program = "systemctl";
                    args = [ "is-system-running" "--wait" ];
                  };
                  timeout = 60;
                }
              ];
            };
          })
        ];
      };

      nixosConfigurations = {
        app-standby = mkHost "app-standby" 1;
        app-primary = mkHost "app-primary" 2;
      };
    in
    {
      inherit nixosConfigurations;
      cattix = cattix.lib.mkFleet {
        hosts = {
          inherit (nixosConfigurations) app-standby app-primary;
        };
      };
    };
}
