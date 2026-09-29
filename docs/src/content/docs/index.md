---
title: Cattix documentation
description: Fleet management and health-gated rolling deployments for NixOS.
tableOfContents:
  maxHeadingLevel: 3
---

# Cattix

Cattix manages fleets of NixOS hosts from a Nix flake. It compares the system declared in Git with the system active on each host, plans ordered rollouts, activates one host at a time, checks service health, and rolls back a host when its checks fail.

Cattix is an orchestration layer over Nix and SSH. It does not replace NixOS configuration, provision machines, manage secrets, or partition disks.

The documentation is maintained as one page. A plain Markdown version for language models and other text consumers is published at [`llms.txt`](llms.txt).

## Project status

The CLI MVP is ready for dogfooding. Implemented commands are `groups`, `status`, `diff`, `plan`, `deploy`, and single-host `rollback`. The deployment path includes ordered SSH activation, command/HTTP/TCP/gRPC health checks, automatic rollback after failed post-deployment checks, remote per-host locks, and durable local run records.

`update`, `inventory`, and `serve` are listed in the CLI but currently return an unimplemented error. `scan` uses pinned sbomnix tools to generate SBOMs and vulnerability reports for desired and active system closures. Traffic draining, group hooks, resumable runs, extensions, revision-aware drift classification, scan policy, external inventory integrations, and the Kubernetes/ArgoCD controller are planned. The CLI currently classifies hosts as `in-sync`, `drifted`, or `unreachable` based on system closure paths.

## How it works

The desired fleet is exported by the flake as the root output `cattix`. Cattix selects it explicitly with `.#.cattix`, avoiding Nix's package-prefix fallback if the consuming flake also exports a package named `cattix`. The fleet includes each host's expected NixOS system closure, rollout group and order, SSH target, metadata, and health checks. Cattix reads the active system from `/run/current-system` over SSH. A local invocation builds the desired system, copies it to the target, activates it, confirms the active closure, and runs configured checks. A successful host is followed by the next host in rollout order. If a host fails after activation, Cattix restores the closure that was active before that Cattix activation, verifies the closure, re-runs health checks, and stops the rollout.

Each deployment writes an fsync'd JSON Lines run record to `$XDG_STATE_HOME/cattix/runs`, or `~/.local/state/cattix/runs` when `XDG_STATE_HOME` is unset. Use `--report-dir DIR` to select another location. The record contains desired and observed closures, events, health outcomes, and final status. It is local to the machine running Cattix; it is not a shared database.

## Requirements

- Nix 2.19 or newer with flakes enabled, and a Linux controller for deployment. Cattix uses Nix's exact-root flake reference syntax to select the fleet output reliably.
- OpenSSH client access from the controller to each managed host.
- A NixOS flake that exports a top-level `cattix` fleet output and builds each configured host.
- SSH credentials for the target user. `nix copy` must be able to access the target Nix store for deployment and for copying an active closure back to the controller during `diff`. On that host-to-controller copy, Cattix accepts paths from the explicitly selected SSH store even if they lack signatures trusted by the controller; SSH host identity and credentials are therefore part of the trust boundary.
- The packaged CLI includes `nix` and `nvd` on its runtime `PATH`. If running from Cargo, install Nix and `nvd` separately; `nvd` powers package/version diffs (`cattix diff`).
- When running Cattix through its Nix package, the packaged `cattix-probe-runner` is available for target-local network probes. A plain `cargo run` development build does not package that runner.

The controller and target must currently use the same platform when target-local HTTP, TCP, or gRPC checks need the probe runner. Per-host runner builds for mixed-architecture fleets are planned.

## Add Cattix to a NixOS flake

Add Cattix as a flake input and import its default NixOS module into each managed configuration. The example below configures two GitLab hosts in one group. Lower `order` values deploy first, so the standby host can be updated before the primary.

```nix
{
  description = "GitLab fleet";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    cattixInput.url = "github:Code-Growers/cattix";
  };

  outputs = { self, nixpkgs, cattixInput, ... }:
    let
      system = "x86_64-linux";
      mkHost = { module, order, target }:
        nixpkgs.lib.nixosSystem {
          inherit system;
          modules = [
            cattixInput.nixosModules.default
            module
            ({ ... }: {
              cattix = {
                group = "gitlab";
                inherit order target;
                metadata = {
                  environment = "production";
                  criticality = "critical";
                  owner = "platform";
                };
                healthChecks = [
                  {
                    name = "gitlab-ready";
                    location = "controller";
                    http.url = "https://gitlab.example/-/readiness?all=1";
                    timeout = 300;
                    interval = 2;
                  }
                ];
              };
            })
          ];
        };
    in
    {
      nixosConfigurations = {
        gitlab-standby = mkHost {
          module = ./hosts/gitlab-standby.nix;
          order = 1;
          target = { host = "10.0.0.5"; port = 22; user = "root"; };
        };
        gitlab-primary = mkHost {
          module = ./hosts/gitlab-primary.nix;
          order = 2;
          target = { host = "10.0.0.6"; port = 22; user = "root"; };
        };
      };

      # Reserve the top-level `cattix` output for the fleet model.
      cattix = cattixInput.lib.mkFleet {
        hosts = {
          inherit (self.nixosConfigurations) gitlab-standby gitlab-primary;
        };
      };

      # Expose the Cattix CLI under a distinct package name.
      packages.${system}.cattix-cli = cattixInput.packages.${system}.default;
    };
}
```

The input is called `cattixInput` to distinguish it from the top-level `cattix` fleet output. The CLI is exposed as `packages.<system>.cattix-cli`, keeping the two interfaces explicit: use `nix eval .#.cattix --json` for the fleet model and `nix run .#cattix-cli -- --flake . groups` for the CLI.

`cattixInput.lib.mkFleet` accepts either an explicit `hosts` attribute set or the shorthand `nixosConfigurations` set, but not both. Explicit `hosts` let the names used by Cattix differ from the names of NixOS configurations. Each host key becomes its Cattix host name. The output is a versioned fleet object containing `hosts` and `groups`.

Evaluate the exported model with:

```sh
nix eval .#.cattix --json
```

The Cattix NixOS module supports `cattix.group`, `cattix.order`, `cattix.target`, `cattix.metadata`, and `cattix.healthChecks`. `target.host` is the SSH address; `target.port` defaults to `22`, and `target.user` defaults to `root`. Metadata fields are `environment`, `criticality`, and `owner`. Cattix does not provision SSH keys or users.

## Health checks

Each health check has a stable `name`, an overall `timeout` in seconds (default `300`), a retry `interval` in seconds (default `1`), and exactly one probe: `command`, `http`, `tcp`, or `grpc`. Checks retry until they pass or the overall deadline expires. A probe can set `timeoutMs` to cap one attempt. `location` may be `controller` or `target`; command checks default to the target and network checks default to the controller.

```nix
healthChecks = [
  {
    name = "system-running";
    location = "target";
    command = {
      program = "systemctl";
      args = [ "is-system-running" "--wait" ];
      expectedStatus = 0;
    };
    timeout = 300;
  }
  {
    name = "local-http-ready";
    location = "target";
    http = {
      url = "http://127.0.0.1/health";
      expectedStatus = 200;
      expectedBody = { type = "contains"; value = "ready"; };
    };
    timeout = 60;
    interval = 2;
  }
  {
    name = "database-port";
    location = "controller";
    tcp.addr = "db.example:5432";
    timeout = 30;
  }
  {
    name = "grpc-health";
    location = "controller";
    grpc = { url = "http://api.example:50051"; service = "my.api.v1.Service"; };
    timeout = 30;
  }
];
```

Command checks execute a program with an argument vector, without parsing a shell command string. They can match the exit code (default `0`) and optionally match stdout or stderr. String matchers are `{ type = "exact" | "exactInsensitive" | "contains"; value = "…"; }`.

HTTP checks support a method (default `get`), an exact expected status or inclusive `expectedStatusRange` (mutually exclusive; default expected status is `200`), expected response headers, a UTF-8 body matcher, and a JSONPath matcher for JSON responses. They can also set a proxy or `tlsInsecure`. Header names are matched case-insensitively. `tlsInsecure` accepts invalid TLS certificates and should only be used when that behavior is intended.

Target-local HTTP, TCP, and gRPC checks run in the target's network namespace through `cattix-probe-runner`, which the packaged application copies to the target. Controller checks run from the machine invoking Cattix. Failed attempts are included in the step tree and structured event output before the next retry.

## CLI reference

Global options go before the command. `--flake` defaults to the current directory. `--impure` allows impure Nix evaluation for controlled fixtures. `--json` requests machine-readable output. `--report-dir DIR` changes the destination for deployment run records.

```sh
cattix --flake . groups
cattix --flake . status
cattix --flake . status --host gitlab-standby
cattix --flake . status --group gitlab
cattix --flake . diff --group gitlab
cattix --flake . plan --group gitlab
cattix --flake . deploy --group gitlab
cattix --flake . deploy --host gitlab-standby --active-closure-timeout 45m
cattix --flake . rollback --host gitlab-standby
```

| Command | Behavior |
| --- | --- |
| `groups` | List rollout groups and their hosts. |
| `status [--host HOST | --group GROUP]` | Compare active and expected system closures; reports `in-sync`, `drifted`, or `unreachable`. With no scope, inspect the full fleet. |
| `diff [--host HOST | --group GROUP]` | Build the expected closure if needed, copy the active closure from the target if needed, then show the `nvd` package/version diff. |
| `plan [--host HOST | --group GROUP]` | Show the selected rollout order and steps without deploying. |
| `deploy [--host HOST | --group GROUP] [--dry-run] [--force]` | Build, copy, activate, and health-check selected hosts in order; a failed post-activation check triggers rollback and stops the rollout. `--dry-run` previews closure changes without modifying hosts. `--force` deploys even when active and expected closure paths match. With no scope, deploy the full fleet. |
| `rollback --host HOST` | Activate and health-check the closure saved before the most recent Cattix activation on that host. |
| `scan` | Generates CycloneDX and SPDX SBOMs, an SBOM CSV, SARIF findings, and scanner evidence for the desired closure. Scans the active closure if it differs. |
| `update`, `inventory`, `serve` | Reserved CLI commands; not implemented yet. |

`--host` and `--group` are mutually exclusive. Deployment and rollback wait up to 20 minutes for the active closure by default. `--active-closure-timeout` accepts seconds, minutes, or hours, such as `45m`. Interactive deployments use a fixed terminal viewport: colored step updates stay above a one-third-height log pane, and the pane follows new build/copy/check lines by default. Use arrow keys, Page Up/Down, Home/End, or the mouse wheel to inspect earlier lines. Older lines move out of view without scrolling the terminal. The interactive screen is restored when the command exits. Colors respect terminal settings such as `NO_COLOR`. JSON output and piped event output retain the complete structured event stream.

## Deployment behavior

Hosts are processed one at a time, sorted by rollout group, host name, and configured order. Hosts already running the expected closure are skipped. For each host that needs an update, Cattix:

1. Builds the configured system with Nix.
2. Acquires the remote lock at `/run/cattix-deployment.lock`; a concurrent deployment is rejected with the lock owner's identifier.
3. Copies the closure to the host with `nix copy`.
4. Saves the closure active immediately before this activation, registers the new generation, and runs `switch-to-configuration switch`.
5. Reconnects and confirms `/run/current-system` matches the desired closure.
6. Runs every configured health check.
7. Releases the lock and continues when checks pass.

If activation or post-deployment checks fail after a switch, Cattix restores the saved closure, confirms it is active, runs the host's checks against the restored system, records rollback outcomes, and stops before updating later hosts. Cattix does not currently drain traffic or run group hooks around activation.

`deploy --dry-run` reads active closures, builds expected closures locally, and shows `nvd` diffs for hosts that would change. It does not acquire remote deployment locks, copy the expected closure to a host, activate a system, or run health checks. Local Nix builds and imports of active closure paths are still possible; this is a preview, not a guarantee that a later deployment will succeed. Combine it with `--force` to preview a forced activation of every selected host, including hosts with no closure difference.

`deploy --force` bypasses the no-change skip and runs the regular build, lock, copy, activation, and health-check flow for every selected host. Use it to re-run activation or health checks when the expected system closure is already active.

The `diff` command runs `nix build` for the expected host configuration so the desired closure is available locally, copies the active closure from the host's Nix store with `nix copy --from`, then runs `nvd` on the controller. This can take time and local disk space when either closure is not already present. The current drift classification uses active closure paths; the fleet includes `system.configurationRevision`, but Cattix does not query that revision from hosts yet. The QEMU NixOS integration scenario covers a two-host rollout, idempotency, manual rollback, automatic rollback, rollback health failure, and lock contention.

## Manual VM lab

The repository provides a disposable two-VM environment:

```sh
make vm-lab
```

The first terminal prints a command to source a generated environment file. In a second terminal, use the exact file path printed by the lab:

```sh
source /tmp/cattix-vm-lab.*/environment
nix develop -c cargo run --bin cattix -- --flake "$CATTIX_VM_FLEET" --impure status
nix develop -c cargo run --bin cattix -- --flake "$CATTIX_VM_FLEET" --impure deploy --group app
```

The lab provides a test-only SSH key to Cattix and `nix copy`. Stop the VM lab with Ctrl-C in its terminal when finished. Run `make help` for all project, documentation, and VM-lab commands.

## Development

The repository is a Rust workspace. The main crates are:

| Crate | Responsibility |
| --- | --- |
| `cattix-core` | Fleet model, validation, planning, health checks, and rollout behavior. |
| `cattix-nix` | Nix evaluation/build/copy and `nvd` process adapter. |
| `cattix-transport` | OpenSSH process adapter. |
| `cattix-utils` | Shared value types and low-level helpers. |
| `cattix-cli` | Clap CLI and terminal/JSON event rendering. |

Run `make help` for common project commands. Rust workflows include `make build`, `make test`, `make fmt`, and `make verify` (format check, tests, and Clippy). `make package` builds the Nix CLI package; `make flake-check` runs all Nix checks, including the longer NixOS integration test. Run the local CLI with `make run ARGS="--flake PATH COMMAND"`. A plain Cargo run does not provide the packaged `cattix-probe-runner` needed for target-local network probes.

## Roadmap and boundaries

Planned work includes traffic draining and enabling, ordered group hooks, resumable deployments, distributed leases for a controller, revision-aware drift, vulnerability severity policy and expiring exceptions, update reports and merge requests, NetBox inventory sync, Prometheus metrics, and a Kubernetes controller with ArgoCD health integration.

Cattix focuses on fleet inspection and rollout. Use separate tools for machine provisioning (for example OpenTofu), secrets (for example agenix or sops-nix), and disk setup (for example disko or nixos-anywhere).

## GitHub Pages

The documentation lives in `docs/`. Pull requests build the Starlight site; pushes to `main` build and publish it with GitHub Pages Actions. In repository settings, set **Pages → Build and deployment → Source** to **GitHub Actions**. The configured project site URL is `https://code-growers.github.io/cattix/`; the plain Markdown endpoint is `https://code-growers.github.io/cattix/llms.txt`.

To work on the docs locally, run `make docs-install`, then `make docs-dev`. `make docs-build` creates the static site in `docs/dist/` and generates `docs/public/llms.txt` from this page.
