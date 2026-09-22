# cattix

Fleet management for NixOS hosts: inventory, drift detection, updates, vulnerability scanning and health-gated rolling deployments, driven from a Nix flake.

> Status: the Rust CLI foundation, read-only commands, and the first real QEMU deployment path are implemented. Server mode and broader integrations remain planned.

## Why

Existing Nix deployment tools (deploy-rs, colmena, clan) copy a closure and run `switch-to-configuration`. None of them can express how a real service should be rolled out:

- **No service health.** deploy-rs's magic rollback only checks that SSH still works. A GitLab that fails its readiness probe after a switch counts as a successful deploy.
- **No rollout strategy.** There's no way to say "these two hosts are a pair behind HAProxy: drain the standby, switch it, wait until it's healthy, re-enable it, then do the primary."
- **No view of the fleet.** Nothing answers "which hosts run something other than `main`", "what changes if I bump nixpkgs", or "which CVEs affect what is deployed".

cattix fills that gap. It stays a thin layer over Nix and doesn't replace it.

## Principles

- **Git is the desired state.** Rollout groups, health checks and metadata are NixOS options, exported as one flake output.
- **Hosts are the actual state.** cattix reads `/run/current-system` (over SSH or from a metric) and never trusts its own records.
- **No database of its own.** History goes to the systems already in place: Prometheus/Mimir, NetBox, GitLab MRs, S3 for reports.
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

The module also sets `system.configurationRevision` and can export a `cattix_build_info{rev, nixpkgs_rev, toplevel}` metric, so hosts report what they run without cattix having to connect.

## Commands

```
cattix groups                  # list rollout groups and their hosts
cattix status [--host h | --group g] # per host: in sync / behind / drifted / unreachable
cattix diff [--host h | --group g]   # nvd diff of deployed vs git (packages, versions, services)
cattix plan [--host h | --group g]   # show the rollout order and steps without running them
cattix deploy [--group g | --host h] # drain → copy → boot entry → switch → checks → re-enable / rollback
cattix rollback --host h            # activate one host's previous generation, run checks
cattix update [--host h | --group g] [--input i] [--mr] # update a flake input, build, diff, open a merge request
cattix scan [--host h | --group g]  # SBOM + CVE scan of each host's system, with ignore list
cattix inventory [--host h | --group g] [--sync netbox] # push the fields Nix owns into NetBox
cattix serve                   # exporter / controller mode (see below)
```

Every command also supports `--json` output for CI.

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
- The active system closure comes from each host's `/run/current-system`, alongside `system.configurationRevision`.
- Host states:
  - `in-sync`: running the build from git.
  - `behind`: running an older commit of the repository.
  - `drifted`: running a build that matches no known commit, e.g. deployed by hand.
  - `unreachable`.
- `diff` shows package and version changes with `nvd`, and service-level changes (systemd units added, removed or changed).

### Rolling deployments

Each host goes through a state machine, one host at a time within a group. Groups run in order.

- **Diff first:** each selected host's deployed and expected build paths are compared. Hosts already in sync are skipped; `cattix diff` remains the detailed `nvd` report.

```
pending → building → copying → draining → activating → checking → enabling → done
                                              │            │
                                              └────────────┴──► rolling-back → failed
```

- **Build:** locally, on a build host, or taken from a binary cache that CI already filled.
- **Activate:** register the new generation as the next boot entry, then run `switch-to-configuration switch`. A failed switch still leaves a bootable configuration, and rollback is an explicit switch to the previous generation.
- **Health checks:** command, HTTP, TCP, and gRPC probes run on the `controller` or `target`. Target-local network probes use the copied runner. Commands receive an executable and argv rather than a shell string. Every probe retries until its deadline and may set a per-attempt `timeoutMs`. HAProxy and custom probes remain planned.
- **Draining:** the HAProxy runtime API first; the interface allows other load balancers later.
- **Failure:** roll back the host, re-run the checks, re-enable it if it's healthy, and stop the rollout. Never continue to the next host.
- **Interruption handling:** after dispatching a switch, Cattix reconnects and confirms the active system instead of assuming failure. A later deterministic deploy repeats the normal diff and is a no-op when that system is already active.
- **Audit log:** each step is emitted as JSON lines for callers to capture and inspect.
- **Hooks:** the plan can include pre/post steps per group. Example: GitLab upgrades that need post-deployment migrations run once, after every node in the group is done.

### Updates

- `cattix update --input nixpkgs` updates one input on its own branch, builds every affected host, and writes a report:
  - package and version diff per host
  - CVEs fixed and introduced, from `scan`
  - which hosts will restart which services
- `--mr` pushes the branch and opens a GitLab or GitHub merge request with the report in its description.
- Each input is updated separately, so pins with different release cadences (e.g. stable nixpkgs vs master for one service) get separate MRs.
- A check flags new NixOS releases so release upgrades are planned.

### Vulnerability scanning

- Create a CycloneDX SBOM from each host's system with `sbomnix`, then scan it with `grype` and `vulnix` (OSV and NVD data).
- An ignore list in the repository (`cattix/vuln-ignore.yaml`) requires a reason and an expiry for every entry. Nixpkgs often patches a CVE without bumping the version, so without this list scanners report fixed CVEs and people stop reading the results.
- Scan both the build from git and what's deployed on each host.
- Outputs:
  - JSON report and SBOM files
  - Prometheus metrics (`cattix_vulnerabilities{host, severity}`)
  - optional upload to Dependency-Track
- Exits non-zero when a new critical or high CVE isn't on the ignore list.

### Inventory

- Syncs host facts from the flake into NetBox: name, platform, NixOS release, nixpkgs revision, service versions, desired and active system closures, last deployment.
- Only updates the fields Nix owns, and never deletes objects or overwrites fields people maintain.

## Architecture

A Rust workspace:

```
cattix-nix        leaf adapter: nix eval/build/copy and nvd process calls
cattix-transport  leaf adapter: OpenSSH process calls
cattix-utils      shared value-type macro and low-level utilities
cattix-core       fleet contract, validation, rollout state machine and application service
cattix-checks     health check trait + built-in kinds
cattix-extension-api versioned API for external checks, drains and hooks
cattix-extensions-haproxy optional HAProxy drain and health-check extension
cattix-scan       SBOM, scanners, ignore list, report model
cattix-sinks      NetBox, GitLab/GitHub MRs, Prometheus, Dependency-Track
cattix-cli        clap frontend and terminal/JSON event rendering
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
| Load balancer draining | no | no | no | no | yes |
| Automatic rollback | on SSH loss | no | no (boot entry first) | no | on failed checks |
| Drift detection | no | no | no | no | yes |
| Update MRs with diff | no | no | no | no | yes |
| Vulnerability scanning | no | no | no | no | yes |

cattix does not do provisioning (use OpenTofu), secrets (use agenix or sops-nix) or disk setup (use disko or nixos-anywhere).

## Roadmap

1. **Fleet model and status:** NixOS module, `mkFleet`, `status`, `diff`.
2. **Deploy:** state machine, SSH transport, command/HTTP checks, rollback, `plan`.
3. **Drain:** generic drain interface, then HAProxy as an optional extension, `haproxy-up` check and group hooks.
4. **Extensions:** versioned Rust extension API, explicit registration of external crates, capability checks and extension audit events.
5. **Scan:** sbomnix + grype/vulnix, ignore list, JSON and metrics output.
6. **Update:** per-input updates, closure diff report, GitLab MR creation.
7. **Inventory:** NetBox sync.
8. **Controller:** CRDs, kube-rs controller, ArgoCD health checks, `/metrics`.

## Open questions

- HTTP checks from the controller vs from the host: which is the default?
- Multi-node migrations (e.g. GitLab zero-downtime upgrades): a built-in strategy or only generic hooks?
- Which binary cache to support first: S3 or attic?
