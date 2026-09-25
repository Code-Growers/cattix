{
  pkgs,
  package,
  nixpkgs,
}:

let
  sshKeys = import (pkgs.path + "/nixos/tests/ssh-keys.nix") pkgs;

  fleetFlakeText = pkgs.writeText "cattix-test-flake.nix" ''
    {
      outputs = _:
        let
          host = name: order: build: address: {
            inherit name order;
            group = "app";
            target = { host = name; port = 22; user = "root"; };
            metadata = {};
            extensions = {};
            healthChecks = [
              {
                name = "sshd";
                location = "target";
                command = {
                  program = "systemctl";
                  args = [ "is-active" "sshd.service" ];
                  expectedStdout = { type = "exact"; value = "active\n"; };
                };
                timeout = 10;
              }
              {
                name = "controller-command";
                location = "controller";
                command = {
                  program = "${pkgs.coreutils}/bin/printf";
                  args = [ "controller-ready" ];
                  expectedStdout = { type = "exact"; value = "controller-ready"; };
                };
                timeout = 10;
              }
              {
                name = "ssh-tcp";
                tcp = { addr = "''${address}:22"; timeoutMs = 1000; };
                timeout = 10;
              }
              {
                name = "nginx-http";
                http = {
                  url = "http://''${address}/health";
                  expectedStatusRange = { min = 200; max = 299; };
                  expectedBody = { type = "contains"; value = "healthy"; };
                  timeoutMs = 1000;
                };
                timeout = 10;
              }
              {
                name = "target-sshd-tcp";
                location = "target";
                tcp = { addr = "127.0.0.1:22"; timeoutMs = 1000; };
                timeout = 10;
              }
              {
                name = "target-nginx-http";
                location = "target";
                http = {
                  url = "http://127.0.0.1/health";
                  expectedBody = { type = "contains"; value = "healthy"; };
                  timeoutMs = 1000;
                };
                timeout = 10;
              }
              {
                name = "target-nginx-json";
                location = "target";
                http = {
                  url = "http://127.0.0.1/health-json";
                  expectedHeaders.Content-Type = { type = "exact"; value = "application/json"; };
                  expectedJson = {
                    path = "$.status";
                    matcher = { type = "exact"; value = "ready"; };
                  };
                  timeoutMs = 1000;
                };
                timeout = 10;
              }
            ];
            expectedBuild = build;
            configurationRevision = "deployed-vm";
          };
          nixosConfigurations = {
            app-standby.config.system.build.toplevel = builtins.storePath "${deploymentBuild "app-standby" "192.168.1.2"}";
            app-primary.config.system.build.toplevel = builtins.storePath "${deploymentBuild "app-primary" "192.168.1.1"}";
          };
        in {
          inherit nixosConfigurations;
          cattix = {
            version = 1;
            hosts = [
              (host "app-standby" 1 nixosConfigurations.app-standby.config.system.build.toplevel "192.168.1.2")
              (host "app-primary" 2 nixosConfigurations.app-primary.config.system.build.toplevel "192.168.1.1")
            ];
            groups.app.hosts = [ "app-standby" "app-primary" ];
          };
        };
    }
  '';

  deploymentBuild = name: address: (nixpkgs.lib.nixosSystem {
    system = pkgs.system;
    modules = [
      ({ ... }: {
        system.stateVersion = "24.11";
        system.configurationRevision = "deployed-vm";
        documentation.enable = false;
        environment.defaultPackages = [ ];
        networking.hostName = name;
        networking.interfaces.eth0.useDHCP = true;
        networking.interfaces.eth1.ipv4.addresses = [
          {
            inherit address;
            prefixLength = 24;
          }
        ];
        networking.firewall.allowedTCPPorts = [ 80 ];
        fileSystems."/" = {
          device = "/dev/vda";
          fsType = "ext4";
        };
        boot.loader.grub.enable = false;
        boot.loader.grub.devices = [ "/dev/vda" ];
        services.openssh.enable = true;
        services.nginx = {
          enable = true;
          virtualHosts.health.locations."/health".return = "200 healthy";
          virtualHosts.health.locations."/health-json".extraConfig = "default_type application/json; return 200 '{ \"status\": \"ready\" }';";
        };
        users.users.root.openssh.authorizedKeys.keys = [ sshKeys.snakeOilPublicKey ];
        environment.etc."cattix-deployed".text = "activated by cattix";
      })
    ];
  }).config.system.build.toplevel;

  failingDeploymentBuild = name: address: (nixpkgs.lib.nixosSystem {
    system = pkgs.system;
    modules = [
      ({ ... }: {
        system.stateVersion = "24.11";
        system.configurationRevision = "failing-vm";
        documentation.enable = false;
        environment.defaultPackages = [ ];
        networking.hostName = name;
        networking.interfaces.eth0.useDHCP = true;
        networking.interfaces.eth1.ipv4.addresses = [
          {
            inherit address;
            prefixLength = 24;
          }
        ];
        fileSystems."/" = {
          device = "/dev/vda";
          fsType = "ext4";
        };
        boot.loader.grub.enable = false;
        boot.loader.grub.devices = [ "/dev/vda" ];
        services.openssh.enable = true;
        users.users.root.openssh.authorizedKeys.keys = [ sshKeys.snakeOilPublicKey ];
        environment.etc."cattix-failed".text = "this generation must be rolled back";
      })
    ];
  }).config.system.build.toplevel;

  failingFleetFlakeText = pkgs.writeText "cattix-failing-test-flake.nix" ''
    {
      outputs = _:
        let
          build = builtins.storePath "${failingDeploymentBuild "app-primary" "192.168.1.1"}";
        in {
          nixosConfigurations.app-primary.config.system.build.toplevel = build;
          cattix = {
            version = 1;
            hosts = [{
              name = "app-primary";
              group = "app";
              order = 1;
              target = { host = "app-primary"; port = 22; user = "root"; };
              metadata = {};
              extensions = {};
              healthChecks = [{
                name = "must-fail";
                location = "target";
                command = { program = "systemctl"; args = [ "is-active" "not-a-service.service" ]; };
                timeout = 2;
              }];
              expectedBuild = build;
              configurationRevision = "failing-vm";
            }];
            groups.app.hosts = [ "app-primary" ];
          };
        };
    }
  '';

  commonNode = {
    system.stateVersion = "24.11";
    documentation.enable = false;
    environment.defaultPackages = [ ];
    fileSystems."/" = {
      device = "/dev/vda";
      fsType = "ext4";
    };
    virtualisation.diskSize = 4096;
    boot.loader.grub.enable = false;
    boot.loader.grub.devices = [ "/dev/vda" ];
    services.openssh.enable = true;
    users.users.root.openssh.authorizedKeys.keys = [ sshKeys.snakeOilPublicKey ];
    virtualisation.memorySize = 1024;
  };

  targetNode = name: order: {
    imports = [ commonNode ];
    networking.hostName = name;
    system.configurationRevision = "vm-test";
    cattix = {
      group = "app";
      inherit order;
      target = {
        host = name;
        port = 22;
        user = "root";
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
    networking.firewall.allowedTCPPorts = [ 80 ];
    services.nginx = {
      enable = true;
      virtualHosts.health.locations."/health".return = "200 healthy";
      virtualHosts.health.locations."/health-json".extraConfig = "default_type application/json; return 200 '{ \"status\": \"ready\" }';";
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "cattix-cli";
  skipTypeCheck = true;
  skipLint = true;

  nodeDefaults.imports = [
    (import ../../nix/cattix-module.nix)
  ];

  nodes = {
    controller = {
      imports = [ commonNode ];

      # The failure fixture is evaluated by the controller during the test, so
      # its pre-built system closure must be available in its Nix store.
      system.extraDependencies = [
        (failingDeploymentBuild "app-primary" "192.168.1.1")
      ];

      environment.systemPackages = [ package pkgs.nix pkgs.openssh ];
      environment.etc."cattix-test-flake.nix".source = fleetFlakeText;
      environment.etc."ssh/cattix-test-key" = {
        source = sshKeys.snakeOilPrivateKey;
        mode = "0600";
      };
      environment.etc."ssh/ssh_config".text = ''
        Host *
          IdentityFile /etc/ssh/cattix-test-key
          StrictHostKeyChecking no
          UserKnownHostsFile /dev/null
      '';
      nix.settings.experimental-features = [ "nix-command" "flakes" ];
    };

    app-standby = targetNode "app-standby" 1;
    app-primary = targetNode "app-primary" 2;
  };

  testScript = ''
    import json

    start_all()
    controller.wait_for_unit("multi-user.target")
    app_standby.wait_for_unit("sshd.service")
    app_primary.wait_for_unit("sshd.service")
    controller.succeed(
        "mkdir -p /tmp/cattix-test && cp /etc/cattix-test-flake.nix /tmp/cattix-test/flake.nix"
    )

    plan = json.loads(controller.succeed(
        "cattix --flake /tmp/cattix-test --impure --json plan --group app"
    ))
    assert [item["host"] for item in plan] == ["app-standby", "app-primary"]
    assert [item["position"] for item in plan] == [1, 2]

    status = json.loads(controller.succeed(
        "cattix --flake /tmp/cattix-test --impure --json status"
    ))
    assert {item["host"] for item in status} == {"app-standby", "app-primary"}
    assert all(item["error"] is None for item in status)
    assert all(item["current_build"].startswith("/nix/store/") for item in status)

    deploy = controller.succeed(
        "cattix --flake /tmp/cattix-test --impure --json deploy --group app"
    )
    events = [json.loads(line) for line in deploy.splitlines()]
    assert events[0]["step"] == "diffing"
    assert events[0]["status"] == "started"
    assert events[-1]["step"] == "finalizing"
    assert events[-1]["status"] == "done"
    controller.succeed("ssh app-standby test -f /etc/cattix-deployed")
    controller.succeed("ssh app-primary test -f /etc/cattix-deployed")

    previous_primary = controller.succeed("ssh app-primary readlink /run/current-system").strip()
    controller.succeed("ssh app-primary 'mkdir /run/cattix-deployment.lock && printf held-by-test > /run/cattix-deployment.lock/owner'")
    locked = controller.fail(
        "cattix --flake /tmp/cattix-test --impure --json rollback --host app-primary"
    )
    assert "cattix deployment lock is held by held-by-test" in locked
    controller.succeed("ssh app-primary 'rm /run/cattix-deployment.lock/owner && rmdir /run/cattix-deployment.lock'")

    manual_rollback = controller.succeed(
        "cattix --flake /tmp/cattix-test --impure --json rollback --host app-primary"
    )
    manual_events = [json.loads(line) for line in manual_rollback.splitlines()]
    assert any(event["step"] == "verifying-rollback" and event["status"] == "done" for event in manual_events)
    assert controller.succeed("ssh app-primary readlink /run/current-system").strip() != previous_primary

    controller.succeed(
        "cattix --flake /tmp/cattix-test --impure --json deploy --host app-primary"
    )
    assert controller.succeed("ssh app-primary readlink /run/current-system").strip() == previous_primary

    skipped = controller.succeed(
        "cattix --flake /tmp/cattix-test --impure --json deploy --group app"
    )
    skipped_events = [json.loads(line) for line in skipped.splitlines()]
    assert all(event["step"] == "diffing" for event in skipped_events)
    skipped_done = [event for event in skipped_events if event["status"] == "done"]
    assert [event["host"] for event in skipped_done] == ["app-standby", "app-primary"]
    assert all(event["detail"] == "no changes; skipping deployment" for event in skipped_done)

    controller.succeed(
        "mkdir -p /tmp/cattix-failing-test && cp ${failingFleetFlakeText} /tmp/cattix-failing-test/flake.nix"
    )
    failed = controller.fail(
        "cattix --flake /tmp/cattix-failing-test --impure --json deploy --host app-primary"
    )
    failed_events = [json.loads(line) for line in failed.splitlines()]
    assert any(event["step"] == "checking" and event["status"] == "failed" for event in failed_events)
    assert any(event["step"] == "rolling-back" and event["status"] == "done" for event in failed_events)
    assert any(event["step"] == "verifying-rollback" and event["status"] == "failed" for event in failed_events)
    controller.succeed("ssh app-primary test -f /etc/cattix-deployed")
    controller.fail("ssh app-primary test -f /etc/cattix-failed")
    app_standby.crash()
    app_primary.crash()
  '';
}
