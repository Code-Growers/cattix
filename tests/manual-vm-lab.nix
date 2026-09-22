{
  pkgs,
  nixpkgs,
}:

let
  sshKeys = import (pkgs.path + "/nixos/tests/ssh-keys.nix") pkgs;

  commonSystem = name: {
    system.stateVersion = "24.11";
    documentation.enable = false;
    environment.defaultPackages = [ ];
    networking.hostName = name;
    networking.interfaces.eth0.useDHCP = true;
    fileSystems."/" = {
      device = "/dev/vda";
      fsType = "ext4";
    };
    boot.loader.grub.enable = false;
    boot.loader.grub.devices = [ "/dev/vda" ];
    services.openssh.enable = true;
    users.users.root.openssh.authorizedKeys.keys = [ sshKeys.snakeOilPublicKey ];
    virtualisation.vmVariant.virtualisation.memorySize = 1024;
  };

  deploymentBuild = name: (nixpkgs.lib.nixosSystem {
    system = pkgs.system;
    modules = [
      (commonSystem name)
      ({ ... }: {
        system.configurationRevision = "cattix-vm-lab-deployed";
        environment.etc."cattix-deployed".text = "activated by cattix";
      })
    ];
  }).config.system.build.toplevel;

  vm = name: port: (nixpkgs.lib.nixosSystem {
    system = pkgs.system;
    modules = [
      (commonSystem name)
      ({ ... }: {
        system.configurationRevision = "cattix-vm-lab-base";
        virtualisation.vmVariant.virtualisation = {
          diskSize = 4096;
          forwardPorts = [
            {
              from = "host";
              host.port = port;
              guest.port = 22;
            }
          ];
        };
      })
    ];
  }).config.system.build.vm;

  appStandbyVm = vm "app-standby" 2223;
  appPrimaryVm = vm "app-primary" 2224;

  fleet = pkgs.writeTextDir "flake.nix" ''
    {
      outputs = _:
        let
          host = name: order: build: {
            inherit name order;
            group = "app";
            target = { host = "127.0.0.1"; port = 2222 + order; user = "root"; };
            metadata = {};
            extensions = {};
            healthChecks = [
              {
                name = "sshd";
                command = { program = "systemctl"; args = [ "is-active" "sshd.service" ]; };
                timeout = 10;
              }
            ];
            expectedBuild = build;
            configurationRevision = "cattix-vm-lab-deployed";
          };
          nixosConfigurations = {
            app-standby.config.system.build.toplevel = builtins.storePath "${deploymentBuild "app-standby"}";
            app-primary.config.system.build.toplevel = builtins.storePath "${deploymentBuild "app-primary"}";
          };
        in {
          inherit nixosConfigurations;
          cattix = {
            version = 1;
            hosts = [
              (host "app-standby" 1 nixosConfigurations.app-standby.config.system.build.toplevel)
              (host "app-primary" 2 nixosConfigurations.app-primary.config.system.build.toplevel)
            ];
            groups.app.hosts = [ "app-standby" "app-primary" ];
          };
        };
    }
  '';
in
pkgs.writeShellApplication {
  name = "cattix-vm-lab";
  runtimeInputs = [ pkgs.coreutils pkgs.openssh pkgs.util-linux ];
  text = ''
    runtime_dir="$(mktemp -d -t cattix-vm-lab.XXXXXX)"
    ssh_config="$runtime_dir/ssh_config"
    environment_file="$runtime_dir/environment"
    standby_pid=""
    primary_pid=""

    cleanup() {
      for pid in "$standby_pid" "$primary_pid"; do
        [ -n "$pid" ] || continue
        kill -TERM -- "-$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
      done
    }
    trap cleanup EXIT INT TERM

    install -m 600 ${sshKeys.snakeOilPrivateKey} "$runtime_dir/id_ed25519"
    printf '%s\n' \
      'Host 127.0.0.1' \
      "  IdentityFile $runtime_dir/id_ed25519" \
      '  IdentitiesOnly yes' \
      '  StrictHostKeyChecking no' \
      '  UserKnownHostsFile /dev/null' \
      > "$ssh_config"
    printf 'export CATTIX_SSH_CONFIG=%q\n' "$ssh_config" > "$environment_file"
    printf 'export NIX_SSHOPTS=%q\n' "-F $ssh_config" >> "$environment_file"
    printf 'export CATTIX_VM_FLEET=%q\n' ${fleet} >> "$environment_file"

    mkdir "$runtime_dir/app-standby" "$runtime_dir/app-primary"
    setsid sh -c "cd \"\$1\"; exec \"\$2\"" sh \
      "$runtime_dir/app-standby" \
      ${appStandbyVm}/bin/run-app-standby-vm &
    standby_pid="$!"
    setsid sh -c "cd \"\$1\"; exec \"\$2\"" sh \
      "$runtime_dir/app-primary" \
      ${appPrimaryVm}/bin/run-app-primary-vm &
    primary_pid="$!"

    wait_for_ssh() {
      port="$1"
      for _ in $(seq 1 60); do
        if ! kill -0 "$standby_pid" 2>/dev/null || ! kill -0 "$primary_pid" 2>/dev/null; then
          echo "a VM exited before SSH became available" >&2
          return 1
        fi
        if ssh -F "$ssh_config" -o BatchMode=yes -o ConnectTimeout=1 -p "$port" root@127.0.0.1 true; then
          return 0
        fi
        sleep 1
      done
      return 1
    }

    wait_for_ssh 2223
    wait_for_ssh 2224

    printf '\nVM lab is ready. In another terminal run:\n\n'
    printf '  source %q\n' "$environment_file"
    printf "  cargo run --bin cattix -- --flake \"\$CATTIX_VM_FLEET\" --impure status\n"
    printf "  cargo run --bin cattix -- --flake \"\$CATTIX_VM_FLEET\" --impure deploy --group app\n\n"
    printf 'Press Ctrl-C here to stop both VMs.\n'
    wait
  '';
}
