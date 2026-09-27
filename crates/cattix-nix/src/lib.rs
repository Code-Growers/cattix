//! Thin process adapter for Nix and Nix-adjacent command-line tools.

use std::{
    io::{BufRead, BufReader, Read},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};

use anyhow::{anyhow, bail, Context, Result};

#[derive(Debug, Default)]
pub struct Nix;

impl Nix {
    /// Evaluates a flake attribute and returns its JSON document unchanged.
    /// Interpretation of that document belongs to the calling application.
    pub fn eval_json(&self, flake: &str, attribute: &str, impure: bool) -> Result<Vec<u8>> {
        let expression = exact_flake_attribute(flake, attribute);
        let mut args = vec!["eval", expression.as_str(), "--json"];
        if impure {
            args.push("--impure");
        }

        let output = Command::new("nix")
            .args(args)
            .output()
            .with_context(|| format!("failed to start nix eval {expression}"))?;

        if !output.status.success() {
            return Err(anyhow!(
                "nix eval {expression} --json failed: {}",
                command_stderr(&output.stderr)
            ));
        }

        Ok(output.stdout)
    }

    pub fn nvd_diff(
        &self,
        active_system_closure: &str,
        desired_system_closure: &str,
    ) -> Result<String> {
        let command = format!("nvd diff {active_system_closure} {desired_system_closure}");
        let output = Command::new("nvd")
            .args(["diff", active_system_closure, desired_system_closure])
            .output()
            .with_context(|| format!("failed to start {command}"))?;

        if !output.status.success() {
            return Err(anyhow!(
                "{command} failed with status {}: stdout: {}; stderr: {}",
                output.status.code().unwrap_or(255),
                command_stderr(&output.stdout),
                command_stderr(&output.stderr)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn build_host_with_logs(
        &self,
        flake: &str,
        host_name: &str,
        impure: bool,
        mut on_log: impl FnMut(&str),
    ) -> Result<String> {
        let attribute = format!("nixosConfigurations.{host_name}.config.system.build.toplevel");
        let expression = exact_flake_attribute(flake, &attribute);
        let mut args = vec![
            "build",
            expression.as_str(),
            "--no-link",
            "--print-out-paths",
        ];
        if impure {
            args.push("--impure");
        }
        let mut child = Command::new("nix")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start nix build {expression}"))?;
        let stdout = child
            .stdout
            .take()
            .context("nix build did not provide stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("nix build did not provide stderr")?;

        let stdout_reader = thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut output = Vec::new();
            BufReader::new(stdout).read_to_end(&mut output)?;
            Ok(output)
        });
        let (sender, receiver) = mpsc::channel();
        let stderr_reader = thread::spawn(move || -> std::io::Result<()> {
            for line in BufReader::new(stderr).lines() {
                if sender.send(line?).is_err() {
                    break;
                }
            }
            Ok(())
        });

        let mut stderr_output = String::new();
        for line in receiver {
            on_log(&line);
            stderr_output.push_str(&line);
            stderr_output.push('\n');
        }

        let status = child.wait().context("waiting for nix build")?;
        stderr_reader
            .join()
            .map_err(|_| anyhow!("nix build stderr reader panicked"))?
            .context("reading nix build stderr")?;
        let stdout = stdout_reader
            .join()
            .map_err(|_| anyhow!("nix build stdout reader panicked"))?
            .context("reading nix build stdout")?;
        if !status.success() {
            return Err(anyhow!(
                "nix build {expression} failed: {}",
                command_stderr(stderr_output.as_bytes())
            ));
        }

        let path = String::from_utf8_lossy(&stdout).trim().to_owned();
        if path.is_empty() {
            bail!("nix build {expression} returned no build path");
        }
        Ok(path)
    }

    pub fn copy_to(&self, build: &str, destination: &str) -> Result<()> {
        let command = format!("nix copy --substitute-on-destination --to {destination} {build}");
        let output = Command::new("nix")
            .args([
                "copy",
                "--substitute-on-destination",
                "--to",
                destination,
                build,
            ])
            .output()
            .with_context(|| format!("failed to start {command}"))?;

        if !output.status.success() {
            return Err(anyhow!(
                "{command} failed with status {}: stdout: {}; stderr: {}",
                output.status.code().unwrap_or(255),
                command_stderr(&output.stdout),
                command_stderr(&output.stderr)
            ));
        }
        Ok(())
    }

    pub fn copy_from(&self, build: &str, source: &str) -> Result<()> {
        // The SSH store is the authenticated source of truth for this active
        // closure. Remote system paths are not necessarily signed with a key
        // trusted by the controller, so accept this explicitly selected
        // source rather than rejecting otherwise valid store paths.
        let command = format!("nix copy --no-check-sigs --from {source} {build}");
        let output = Command::new("nix")
            .args(["copy", "--no-check-sigs", "--from", source, build])
            .output()
            .with_context(|| format!("failed to start {command}"))?;

        if !output.status.success() {
            return Err(anyhow!(
                "{command} failed with status {}: stdout: {}; stderr: {}",
                output.status.code().unwrap_or(255),
                command_stderr(&output.stdout),
                command_stderr(&output.stderr)
            ));
        }
        Ok(())
    }

    /// Copies the packaged target probe runner closure and returns its target path.
    pub fn copy_probe_runner(
        &self,
        destination: &str,
        on_log: &mut dyn FnMut(&str),
    ) -> Result<String> {
        let executable = std::env::current_exe().context("locating cattix executable")?;
        let executable = executable
            .canonicalize()
            .context("resolving cattix executable")?;
        let package = executable
            .parent()
            .and_then(|bin| bin.parent())
            .context("locating cattix package directory")?;
        let runner = package.join("bin/cattix-probe-runner");
        if !runner.is_file() {
            bail!(
                "target-local network probes require a packaged cattix installation containing cattix-probe-runner"
            );
        }
        let package = package
            .to_str()
            .context("cattix package path is not valid UTF-8")?;
        if !package.starts_with("/nix/store/") {
            bail!(
                "target-local network probes require cattix to run from a Nix store package; use nix run or nix build"
            );
        }
        on_log("copying cattix-probe-runner closure");
        self.copy_to(package, destination)?;
        Ok(runner
            .to_str()
            .context("cattix probe runner path is not valid UTF-8")?
            .to_owned())
    }
}

/// Select a flake output attribute without Nix's package-prefix fallback.
/// Fleet data such as `cattix` must remain distinct from installable packages.
fn exact_flake_attribute(flake: &str, attribute: &str) -> String {
    format!("{flake}#.{attribute}")
}

#[cfg(test)]
mod tests {
    use super::exact_flake_attribute;

    #[test]
    fn flake_attributes_are_selected_from_the_output_root() {
        assert_eq!(exact_flake_attribute(".", "cattix"), ".#.cattix");
        assert_eq!(
            exact_flake_attribute(
                "/tmp/fleet",
                "nixosConfigurations.host.config.system.build.toplevel"
            ),
            "/tmp/fleet#.nixosConfigurations.host.config.system.build.toplevel"
        );
    }
}

fn command_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_owned()
}
