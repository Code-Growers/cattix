//! Thin OpenSSH process adapter.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub address: String,
    pub port: u16,
    pub user: String,
}

impl SshTarget {
    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.address)
    }
}

#[derive(Debug)]
pub struct SshOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Default)]
pub struct OpenSsh;

impl OpenSsh {
    pub fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput> {
        self.run_with_input(target, command, &[])
    }

    pub fn run_with_input(
        &self,
        target: &SshTarget,
        command: &str,
        input: &[u8],
    ) -> Result<SshOutput> {
        let destination = target.destination();
        let port = target.port.to_string();
        let mut ssh = Command::new("ssh");
        if let Ok(config) = std::env::var("CATTIX_SSH_CONFIG") {
            ssh.args(["-F", config.as_str()]);
        }
        let mut child = ssh
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-p",
                &port,
                &destination,
                command,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start ssh to {destination}"))?;
        if !input.is_empty() {
            child
                .stdin
                .as_mut()
                .context("ssh did not provide stdin")?
                .write_all(input)
                .with_context(|| format!("writing input to ssh for {destination}"))?;
        }
        let output = child
            .wait_with_output()
            .with_context(|| format!("waiting for ssh to {destination}"))?;

        Ok(SshOutput {
            status: output.status.code().unwrap_or(255),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
