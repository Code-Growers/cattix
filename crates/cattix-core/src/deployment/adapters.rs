use crate::{FlakeRef, Host, SystemClosure};
use anyhow::{anyhow, Result};
use cattix_nix::Nix;
use cattix_transport::{OpenSsh, SshOutput, SshTarget};

/// Internal boundary between the rollout state machine and the build backend.
pub(super) trait Builder {
    fn build(&self, host: &Host, on_log: &mut dyn FnMut(&str)) -> Result<SystemClosure>;
    fn copy_to(&self, build: &SystemClosure, host: &Host) -> Result<()>;
    fn copy_probe_runner(&self, host: &Host, on_log: &mut dyn FnMut(&str)) -> Result<String>;
}

#[derive(Debug)]
pub(super) struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Internal boundary between the rollout state machine and remote execution.
pub(super) trait Transport {
    fn run(&self, host: &Host, command: &str) -> Result<CommandOutput>;
    fn run_with_input(&self, host: &Host, command: &str, input: &[u8]) -> Result<CommandOutput>;
    fn destination(&self, host: &Host) -> Result<String>;
}

pub(super) struct NixDeploymentBackend<'a> {
    pub nix: &'a Nix,
    pub flake: &'a FlakeRef,
    pub impure: bool,
}

impl Builder for NixDeploymentBackend<'_> {
    fn build(&self, host: &Host, on_log: &mut dyn FnMut(&str)) -> Result<SystemClosure> {
        self.nix
            .build_host_with_logs(self.flake.as_str(), host.name.as_str(), self.impure, on_log)
            .map(SystemClosure::new)
    }

    fn copy_to(&self, build: &SystemClosure, host: &Host) -> Result<()> {
        self.nix.copy_to(build.as_str(), &nix_destination(host)?)
    }

    fn copy_probe_runner(&self, host: &Host, on_log: &mut dyn FnMut(&str)) -> Result<String> {
        self.nix.copy_probe_runner(&nix_destination(host)?, on_log)
    }
}

pub(super) struct SshTransport<'a> {
    pub ssh: &'a OpenSsh,
}

impl Transport for SshTransport<'_> {
    fn run(&self, host: &Host, command: &str) -> Result<CommandOutput> {
        self.ssh
            .run(&ssh_target(host)?, command)
            .map(command_output)
    }

    fn run_with_input(&self, host: &Host, command: &str, input: &[u8]) -> Result<CommandOutput> {
        self.ssh
            .run_with_input(&ssh_target(host)?, command, input)
            .map(command_output)
    }

    fn destination(&self, host: &Host) -> Result<String> {
        Ok(ssh_target(host)?.destination())
    }
}

fn ssh_target(host: &Host) -> Result<SshTarget> {
    Ok(SshTarget {
        address: host
            .target
            .host
            .clone()
            .ok_or_else(|| anyhow!("no SSH target configured for {}", host.name))?,
        port: host.target.port,
        user: host.target.user.clone(),
    })
}

pub(super) fn nix_destination(host: &Host) -> Result<String> {
    let target = ssh_target(host)?;
    if target.port == 22 {
        Ok(format!("ssh://{}@{}", target.user, target.address))
    } else {
        Ok(format!(
            "ssh://{}@{}:{}",
            target.user, target.address, target.port
        ))
    }
}

fn command_output(output: SshOutput) -> CommandOutput {
    CommandOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    }
}
