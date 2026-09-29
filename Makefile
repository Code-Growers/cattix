.DEFAULT_GOAL := help

NIX ?= nix
CARGO ?= cargo
NPM ?= npm
ARGS ?= --help
LAB_ENV ?=

.PHONY: help dev build test fmt fmt-check lint verify package flake-check clean run run-installed \
	docs-install docs-dev docs-build docs-preview vm-lab lab-cattix lab-status lab-plan \
	lab-diff lab-deploy lab-deploy-standby lab-deploy-primary lab-rollback-standby

help: ## Show available commands
	@printf '%s\n' \
	  'Development:' \
	  '  make dev                  Enter the Nix development shell' \
	  '  make build                Build all Rust workspace crates' \
	  '  make test                 Run all Rust workspace tests' \
	  '  make fmt                  Format Rust code' \
	  '  make fmt-check            Check Rust formatting without changing files' \
	  '  make lint                 Run Clippy with warnings denied' \
	  '  make verify               Run format check, tests, and Clippy' \
	  '  make package              Build the packaged CLI with Nix' \
	  '  make flake-check          Run all Nix flake checks (including the VM test)' \
	  '  make run ARGS="status"    Run the local CLI; ARGS defaults to --help' \
	  '  make run-installed ARGS="status"  Run the packaged CLI from this flake' \
	  '  make clean                Clean Cargo build artifacts' \
	  '' \
	  'Documentation:' \
	  '  make docs-install         Install docs dependencies with npm ci' \
	  '  make docs-dev             Run the local Starlight development server' \
	  '  make docs-build           Build the Starlight site and /llms.txt' \
	  '  make docs-preview         Preview the built documentation site' \
	  '' \
	  'Two-VM lab:' \
	  '  make vm-lab               Build/start the two local QEMU hosts (Ctrl-C to stop)' \
	  '  make lab-status LAB_ENV=PATH  Inspect both lab hosts' \
	  '  make lab-plan LAB_ENV=PATH    Preview the rollout plan' \
	  '  make lab-diff LAB_ENV=PATH    Compare active and desired closures' \
	  '  make lab-deploy LAB_ENV=PATH  Deploy the app group' \
	  '  make lab-deploy-standby LAB_ENV=PATH  Deploy app-standby only' \
	  '  make lab-deploy-primary LAB_ENV=PATH  Deploy app-primary only' \
	  '  make lab-rollback-standby LAB_ENV=PATH  Roll back app-standby' \
	  '  make lab-cattix LAB_ENV=PATH ARGS="..."  Run any CLI command against the lab'

dev: ## Enter the Nix development shell
	$(NIX) develop

build: ## Build all Rust workspace crates
	$(NIX) develop -c $(CARGO) build --workspace

test: ## Run all Rust workspace tests
	$(NIX) develop -c $(CARGO) test --workspace

fmt: ## Format Rust code
	$(NIX) develop -c $(CARGO) fmt --all

fmt-check: ## Check Rust formatting
	$(NIX) develop -c $(CARGO) fmt --all -- --check

lint: ## Run Clippy with warnings denied
	$(NIX) develop -c $(CARGO) clippy --workspace --all-targets -- -D warnings

verify: fmt-check test lint ## Run the standard Rust verification suite

package: ## Build the installable CLI package with Nix
	$(NIX) build .#default

flake-check: ## Run Nix flake checks, including the NixOS VM integration test
	$(NIX) flake check

run: ## Run the local CLI (override ARGS, e.g. make run ARGS="status")
	$(NIX) develop -c $(CARGO) run --bin cattix -- $(ARGS)

run-installed: ## Run the packaged CLI (override ARGS, e.g. make run-installed ARGS="groups")
	$(NIX) run .#default -- $(ARGS)

clean: ## Clean Cargo build artifacts
	$(NIX) develop -c $(CARGO) clean

docs-install: ## Install documentation dependencies
	$(NIX) develop -c $(NPM) ci --prefix docs

docs-dev: ## Run the local Starlight development server
	$(NIX) develop -c $(NPM) --prefix docs run dev

docs-build: ## Build the Starlight site and generated /llms.txt
	$(NIX) develop -c $(NPM) --prefix docs run build

docs-preview: ## Preview the built Starlight site
	$(NIX) develop -c $(NPM) --prefix docs run preview

vm-lab: ## Start the disposable two-VM deployment lab
	$(NIX) run .#cattix-vm-lab

lab-cattix: ## Run any CLI command against the VM lab (requires LAB_ENV and ARGS)
	@test -n "$(LAB_ENV)" || { echo 'Set LAB_ENV to the environment file printed by make vm-lab' >&2; exit 2; }
	@test -f "$(LAB_ENV)" || { echo 'LAB_ENV does not name a readable lab environment file' >&2; exit 2; }
	@bash -eu -c 'set -a; . "$$1"; shift; set +a; exec $(NIX) develop -c $(CARGO) run --bin cattix -- --flake "$$CATTIX_VM_FLEET" --impure "$$@"' bash "$(LAB_ENV)" $(ARGS)

lab-status: ## Show status of both VM lab hosts
	$(MAKE) --no-print-directory lab-cattix LAB_ENV="$(LAB_ENV)" ARGS=status

lab-plan: ## Preview the VM lab rollout plan
	$(MAKE) --no-print-directory lab-cattix LAB_ENV="$(LAB_ENV)" ARGS=plan

lab-diff: ## Compare active and desired VM lab closures
	$(MAKE) --no-print-directory lab-cattix LAB_ENV="$(LAB_ENV)" ARGS=diff

lab-deploy: ## Deploy the VM lab app group
	$(MAKE) --no-print-directory lab-cattix LAB_ENV="$(LAB_ENV)" ARGS="deploy --group app"

lab-deploy-standby: ## Deploy only app-standby in the VM lab
	$(MAKE) --no-print-directory lab-cattix LAB_ENV="$(LAB_ENV)" ARGS="deploy --host app-standby"

lab-deploy-primary: ## Deploy only app-primary in the VM lab
	$(MAKE) --no-print-directory lab-cattix LAB_ENV="$(LAB_ENV)" ARGS="deploy --host app-primary"

lab-rollback-standby: ## Roll back app-standby in the VM lab
	$(MAKE) --no-print-directory lab-cattix LAB_ENV="$(LAB_ENV)" ARGS="rollback --host app-standby"
