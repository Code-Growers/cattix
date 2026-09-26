# cattix

Fleet management for NixOS hosts: inventory, drift detection, updates, vulnerability scanning and health-gated rolling deployments, driven from a Nix flake.

> Status: the CLI MVP is ready for dogfooding: ordered SSH deployments, health-gated rollback, per-host deployment locks, and durable local run records are implemented. Draining, hooks, extensions, update/scan/inventory, and server mode are not yet implemented.

## Documentation

The [project documentation](docs/src/content/docs/index.md) is a single Starlight page. It includes the Nix flake interface, health checks, CLI, deployment behavior, and current project status. The site also publishes a plain Markdown version at `/llms.txt` for language-model ingestion. GitHub Actions builds the site on pull requests and publishes it to GitHub Pages from `main`.

## Why

Existing Nix deployment tools (deploy-rs, colmena, clan) copy a closure and run `switch-to-configuration`. None of them can express how a real service should be rolled out:

- **No service health.** deploy-rs's magic rollback only checks that SSH still works. A GitLab that fails its readiness probe after a switch counts as a successful deploy.
- **No rollout strategy.** There's no way to say "these two hosts are a pair behind HAProxy: drain the standby, switch it, wait until it's healthy, re-enable it, then do the primary."
- **No view of the fleet.** Nothing answers "which hosts run something other than `main`", "what changes if I bump nixpkgs", or "which CVEs affect what is deployed".

cattix fills that gap. It stays a thin layer over Nix and doesn't replace it.

## Current status

Implemented:

- NixOS fleet options and `cattix.lib.mkFleet`, including explicit host-to-configuration mapping.
- `groups`, `status`, `diff`, `plan`, `deploy`, and single-host `rollback` CLI commands.
- Ordered, one-host-at-a-time deployment: build, copy, activate, confirm the active closure, and run health checks.
- Command, HTTP, TCP, and gRPC health checks from the controller or target; target-local network checks use the packaged probe runner.
- Rollback to the closure saved immediately before a Cattix activation, including automatic rollback after a failed post-deployment check and health verification after every rollback.
- Per-host remote deployment locks, with the current lock owner reported to a concurrent caller.
- fsync'd local JSONL run records containing desired and observed closures, events, health outcomes, and final status.
- A QEMU NixOS integration test covering a successful two-host rollout, idempotency, manual rollback health verification, automatic rollback, rollback-health failure, and lock contention.

Not implemented yet:

- Traffic draining/enabling, group hooks, resumable runs, and distributed leases for a future controller.
- `update`, `scan`, `inventory`, and `serve`; the CLI accepts these commands but returns an unimplemented error.
- Extension execution, SBOM/CVE scanning, NetBox/MR integrations, metrics, and the Kubernetes/ArgoCD controller.
- Revision-aware drift classification: Cattix currently reports only `in-sync`, `drifted`, or `unreachable` from the active closure.

## Principles

- **Git is the desired state.** Rollout groups, health checks and metadata are NixOS options, exported as one flake output.
- **Hosts are the actual state.** cattix reads `/run/current-system` (over SSH or from a metric) and never trusts its own records.
- **No long-lived database of its own.** Each CLI run writes an fsync'd local JSONL record; integrations can later export history to Prometheus/Mimir, NetBox, GitLab MRs, or S3.
- **Shell out to Nix.** Use `nix eval`, `nix build`, `nix copy`, `nvd` and `sbomnix` rather than reimplementing them.
- **Checks live next to the service.** The module that enables GitLab also declares GitLab's health checks.
- **Works without the cluster.** The CLI must work even when Kubernetes, GitLab or Vault are down, because those may be the hosts being repaired.

## Flake interface

A NixOS module adds the `cattix` options:

```nix
cattix = {
  group = "gitlab";        # hosts in a group roll one at a time
  order = 2;               # lower goes first (standby before primary)
  target = { host = "10.0.0.6"; port = 22; user = "root"; };

  metadata = {
    environment = "production";
    criticality = "critical";
    owner = "infra";
  };

  healthChecks = [
    {
      name = "units";
      location = "target";
      command = { program = "systemctl"; args = [ "is-system-running" "--wait" ]; };
      timeout = 300;
    }
    {
      name = "public-readiness";
      location = "controller";
      http.url = "https://gitlab.example/-/readiness?all=1";
      timeout = 900;
    }
    {
      name = "local-nginx";
      location = "target";
      http = {
        url = "http://127.0.0.1/health";
        expectedBody = { type = "contains"; value = "ready"; };
      };
      timeout = 60;
    }
  ];
};
```

A library function collects explicitly selected NixOS configurations into one fleet output. Each
host key becomes the Cattix host name; its value is the corresponding NixOS configuration. Cattix
derives the expected system closure and host-level options itself:

```nix
cattix = cattix.lib.mkFleet {
  hosts = {
    gitlab-standby = self.nixosConfigurations.gitlab1;
    gitlab-primary = self.nixosConfigurations.gitlab2;
  };
};
```

For fleets where every configuration is managed by Cattix, the existing shorthand remains
available: `cattix.lib.mkFleet { inherit (self) nixosConfigurations; }`.

Here is a complete flake shape using explicit hosts. The files in `./hosts/` contain the usual
machine-specific NixOS configuration (hardware, boot, users, and services).

```nix
{
  description = "GitLab fleet";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    cattix.url = "github:your-org/cattix";
  };

  outputs = { self, nixpkgs, cattix, ... }:
    let
      system = "x86_64-linux";
      mkHost = { module, group, order, target }:
        nixpkgs.lib.nixosSystem {
          inherit system;
          modules = [
            cattix.nixosModules.default
            module
            ({ ... }: {
              cattix = {
                inherit group order target;
                metadata = {
                  environment = "production";
                  criticality = "critical";
                  owner = "platform";
                };
                healthChecks = [
                  {
                    name = "gitlab-ready";
                    http.url = "http://127.0.0.1/-/readiness?all=1";
                    timeout = 300;
                  }
                ];
              };
            })
          ];
        };
    in
    {
      nixosConfigurations = {
        gitlab1 = mkHost {
          module = ./hosts/gitlab1.nix;
          group = "gitlab";
          order = 1;
          target = { host = "10.0.0.5"; port = 22; user = "root"; };
        };
        gitlab2 = mkHost {
          module = ./hosts/gitlab2.nix;
          group = "gitlab";
          order = 2;
          target = { host = "10.0.0.6"; port = 22; user = "root"; };
        };
      };

      cattix = cattix.lib.mkFleet {
        hosts = {
          gitlab1 = self.nixosConfigurations.gitlab1;
          gitlab2 = self.nixosConfigurations.gitlab2;
        };
      };
    };
}
```

`nix eval .#cattix --json` returns the whole fleet model: hosts, groups, order, checks, metadata, and the expected system build for each host.

Command, HTTP, TCP, and gRPC probes can run on the controller or target. Target-local network
probes copy `cattix-probe-runner` to the target through Nix, then execute the ZeroNine probe in
the target's network namespace. The runner is included when Cattix is installed or run as its Nix
package (`nix run`); it is not available when running only `cargo run` from a development tree.
For now, the controller and target must use the same platform; per-host runner builds are planned
for mixed-architecture fleets.

HTTP checks can match an exact status (or an inclusive `expectedStatusRange`), response headers,
UTF-8 body text, and a JSONPath value. String matchers use
`{ type = "exact" | "exactInsensitive" | "contains"; value = "…"; }`. Command checks can
apply the same matcher form to `expectedStdout` and `expectedStderr`. Failed attempts include the
probe's error in both the live step tree and structured event output before Cattix retries.

`mkFleet` includes a host's configured `system.configurationRevision` in the fleet document. Cattix does not currently query that revision from hosts or expose a build-info metric.

## Commands

```
cattix groups                  # list rollout groups and their hosts
cattix status [--host h | --group g] # per host: in sync / drifted / unreachable
cattix diff [--host h | --group g]   # nvd diff of deployed vs git (packages, versions, services)
cattix plan [--host h | --group g]   # show the rollout order and steps without running them
cattix deploy [--group g | --host h] [--active-closure-timeout 45m]
                                      # build → copy → switch → checks → rollback on check failure
cattix rollback --host h [--active-closure-timeout 45m]
                                      # activate and health-check the Cattix-saved previous generation
cattix update ...                    # not implemented
cattix scan ...                      # not implemented
cattix inventory ...                 # not implemented
cattix serve                         # not implemented
```

Every command also supports `--json` output for CI. `deploy` and `rollback` default to a
20-minute active-closure wait, which is appropriate for slower services such as GitLab; pass
`--active-closure-timeout 45m` (or a value in `s`, `m`, or `h`) when needed. They write an
fsync'd JSONL run record under `$XDG_STATE_HOME/cattix/runs` (or
`~/.local/state/cattix/runs`); use `--report-dir DIR` to choose another location.

Interactive deployments render a retained tree on stderr. Each host contains its deployment
steps, and build/copy/check output remains directly below the step that produced it. Piped output
and `--json` use structured tracing events instead.

The local QEMU fixture may use `--impure` to reference pre-built test store paths; normal flake evaluation remains pure by default.

## Manual VM lab

Run two local QEMU targets in one terminal:

```sh
nix run .#cattix-vm-lab
```

It prints a `source` command for a generated environment file. In a second terminal, source it and run the local CLI against the two VMs:

```sh
source /tmp/cattix-vm-lab.*/environment
nix develop -c cargo run --bin cattix -- --flake "$CATTIX_VM_FLEET" --impure status
nix develop -c cargo run --bin cattix -- --flake "$CATTIX_VM_FLEET" --impure deploy --group app
nix develop -c cargo run --bin cattix -- --flake "$CATTIX_VM_FLEET" --impure deploy --host app-standby
```

The environment supplies a test-only SSH key to both Cattix and `nix copy`; stop the first terminal with Ctrl-C when finished.

## Features

### Drift detection

- The expected build comes from `nix eval` or `nix build` at a git revision.
- The active system closure comes from each host's `/run/current-system`.
- Host states:
  - `in-sync`: running the build from git.
  - `drifted`: running a different closure.
  - `unreachable`.
- Revision-aware `behind` classification is planned; the fleet's `configurationRevision` is not yet probed from the target.
- `diff` shows the `nvd` report for a changed closure.

### Rolling deployments

Each selected host deploys one at a time, in group/name/order sort order.

- **Diff first:** each selected host's deployed and expected build paths are compared. Hosts already in sync are skipped; `cattix diff` remains the detailed `nvd` report.

```
pending → building → locking → copying → activating → checking → done
                                                 │             │
                                                 └─────────────┴──► rolling-back → rollback-checking → failed
```

- **Build:** locally through `nix build`; Nix may use configured binary caches.
- **Activate:** register the new generation as the next boot entry, then run `switch-to-configuration switch`. A failed switch still leaves a bootable configuration, and rollback is an explicit switch to the previous generation.
- **Health checks:** command, HTTP, TCP, and gRPC probes run on the `controller` or `target`. Target-local network probes use the copied runner. Commands receive an executable and argv rather than a shell string. Every probe retries until its deadline and may set a per-attempt `timeoutMs`. HAProxy and custom probes remain planned.
- **Failure:** roll back the host's closure, confirm its active closure, rerun its health checks, and stop the rollout. Traffic re-enabling is planned with extensions.
- **Interruption handling:** after dispatching a switch, Cattix reconnects and confirms the active system instead of assuming failure. A later deterministic deploy repeats the normal diff and is a no-op when that system is already active.
- **Audit events:** each step is emitted as JSON lines for callers to capture and is fsync'd to a local JSONL run record. Resume support is planned.
- **Draining and hooks:** group hooks, migration strategies, and load-balancer integration are planned.

### Updates (planned)

- `cattix update --input nixpkgs` updates one input on its own branch, builds every affected host, and writes a report:
  - package and version diff per host
  - CVEs fixed and introduced, from `scan`
  - which hosts will restart which services
- `--mr` pushes the branch and opens a GitLab or GitHub merge request with the report in its description.
- Each input is updated separately, so pins with different release cadences (e.g. stable nixpkgs vs master for one service) get separate MRs.
- A check flags new NixOS releases so release upgrades are planned.

### Vulnerability scanning (planned)

- Create a CycloneDX SBOM from each host's system with `sbomnix`, then scan it with `grype` and `vulnix` (OSV and NVD data).
- An ignore list in the repository (`cattix/vuln-ignore.yaml`) requires a reason and an expiry for every entry. Nixpkgs often patches a CVE without bumping the version, so without this list scanners report fixed CVEs and people stop reading the results.
- Scan both the build from git and what's deployed on each host.
- Outputs:
  - JSON report and SBOM files
  - Prometheus metrics (`cattix_vulnerabilities{host, severity}`)
  - optional upload to Dependency-Track
- Exits non-zero when a new critical or high CVE isn't on the ignore list.

### Inventory (planned)

- Syncs host facts from the flake into NetBox: name, platform, NixOS release, nixpkgs revision, service versions, desired and active system closures, last deployment.
- Only updates the fields Nix owns, and never deletes objects or overwrites fields people maintain.

## Architecture

A Rust workspace:

```
cattix-nix        leaf adapter: nix eval/build/copy and nvd process calls
cattix-transport  leaf adapter: OpenSSH process calls
cattix-utils      shared value-type macro and low-level utilities
cattix-core       fleet contract, validation, rollout state machine and application service
cattix-cli        clap frontend and terminal/JSON event rendering

# planned crates
cattix-extension-api versioned API for external checks, drains and hooks
cattix-extensions-haproxy optional HAProxy drain and health-check extension
cattix-scan       SBOM, scanners, ignore list, report model
cattix-sinks      NetBox, GitLab/GitHub MRs, Prometheus, Dependency-Track
cattix-controller kube-rs controller + /metrics (optional)
```

`cattix-core` calls the Nix and SSH adapters. Both the CLI and the future
controller are frontends over its `FleetService`; they do not implement
deployment policy or deserialize flake output themselves.

```
cattix-nix + cattix-transport
              │
              ▼
         cattix-core
          │       │
          ▼       ▼
     cattix-cli  controller
```

```
flake eval ──┐
host probe ──┼──► fleet model ──► planner ──► executor (rollout state machine)
scanners   ──┘         │
                       └──► sinks: NetBox, MRs, metrics, SBOM storage
```

The CLI and the controller use the same core, so every action the controller takes can also be run by hand.

### External extensions (later)

- Extensions use their own Nix module and a namespaced configuration such as `cattix.extensions.haproxy`.
- A small, versioned `cattix-extension-api` defines providers for health checks, traffic draining and rollout hooks.
- The first extension mechanism compiles external Rust crates into a custom cattix binary and registers them explicitly, avoiding an unstable Rust ABI.
- Later, independently released extensions can run as subprocesses over a small JSON protocol; the core remains responsible for planning, permissions, failures and audit events.

## Kubernetes / ArgoCD integration (later)

A controller mode uses custom resources, so ArgoCD's UI can display the fleet:

- **CRDs:**
  - `NixosHost`: spec holds the flake ref and host; status holds the deployed build, sync state, last health result and CVE counts.
  - `NixosRolloutGroup`: ordering, one-at-a-time limit, drain settings.
  - `NixosRollout`: one rollout run, with per-host phases.
- **ArgoCD** syncs the resources from git. Lua health checks for the CRDs make it show hosts as Healthy, Progressing or Degraded.
- **GitOps flow:** a merged update MR bumps the flake revision in the specs, ArgoCD syncs, and the controller rolls the change out.
- **Builds stay in CI,** which pushes to a binary cache. The controller only copies and activates.
- **SSH credentials** come from Vault or a Secret, in a dedicated namespace with restricted network access.
- **Caveat:** ArgoCD's OutOfSync compares resources to git, not hosts to git. Host drift shows up as Degraded health.

## Prior art

| | deploy-rs | colmena | clan | nixops4 | cattix |
|---|---|---|---|---|---|
| Deploy | yes | yes | yes | pre-release | yes |
| Service health checks | SSH only | no | no | no | yes |
| Rolling / ordered rollout | no | parallel limit | no (parallel) | no | yes |
| Load balancer draining | no | no | no | no | planned |
| Automatic rollback | on SSH loss | no | no (boot entry first) | no | closure rollback on failed checks |
| Drift detection | no | no | no | no | closure comparison |
| Update MRs with diff | no | no | no | no | planned |
| Vulnerability scanning | no | no | no | no | planned |

cattix does not do provisioning (use OpenTofu), secrets (use agenix or sops-nix) or disk setup (use disko or nixos-anywhere).

## Roadmap

1. **Done — fleet model and inspection:** NixOS module, `mkFleet`, `groups`, `status`, `diff`, and `plan`.
2. **Done — dogfooding deploy MVP:** sequential Nix/SSH rollout, health-gated rollback, post-rollback checks, remote per-host locks, durable local records, and configurable active-closure wait. Resume and controller-oriented distributed leases remain later work.
3. **Drain and hooks:** generic drain interface, HAProxy extension, `haproxy-up` check, and group hooks.
4. **Extensions:** versioned Rust extension API, explicit registration of external crates, capability checks, and extension audit events.
5. **Scan:** sbomnix + grype/vulnix, ignore list, JSON, and metrics output.
6. **Update:** per-input updates, closure diff report, and GitLab/GitHub MR creation.
7. **Inventory:** NetBox sync.
8. **Controller:** metrics, CRDs, kube-rs controller, and ArgoCD health checks.
