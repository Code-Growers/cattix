use super::{
    adapters::{Builder, CommandOutput, Transport},
    commands::{acquire_lock_command, activation_command, release_lock_command, rollback_command},
    health::{requires_target_probe_runner, run_health_checks},
    DeployEvent, DeployStep, DeploymentOptions, DeploymentReporter, DeploymentScope,
    SystemClosureDiff,
};
use crate::{Fleet, Host, SystemClosure};
use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;

pub(super) fn system_closure_diff(
    transport: &dyn Transport,
    host: &Host,
) -> Result<SystemClosureDiff> {
    let active_system_closure = active_system_closure(transport, host)?;
    Ok(SystemClosureDiff {
        active_system_closure,
        desired_system_closure: host.desired_system_closure.clone(),
    })
}

pub(super) fn active_system_closure(
    transport: &dyn Transport,
    host: &Host,
) -> Result<SystemClosure> {
    let destination = transport.destination(host)?;
    let output = transport
        .run(host, "readlink /run/current-system")
        .with_context(|| format!("reading active system closure from {destination}"))?;
    ensure_success(output).map(|build| SystemClosure::new(build.trim()))
}

pub(super) fn deploy<R: DeploymentReporter>(
    fleet: &Fleet,
    scope: DeploymentScope<'_>,
    builder: &dyn Builder,
    transport: &dyn Transport,
    options: DeploymentOptions,
    reporter: &mut R,
) -> Result<()> {
    for host in fleet.selected_hosts(scope)? {
        let changes_detected = reporter.report_step(
            host.name.as_str(),
            DeployStep::Diffing,
            |_| {
                let diff = system_closure_diff(transport, host)
                    .with_context(|| format!("{}: checking deployment diff", host.name))?;
                Ok(diff.has_changes())
            },
            |changed| {
                if *changed {
                    "changes detected".into()
                } else if options.force {
                    "no changes; forced deployment".into()
                } else {
                    "no changes; skipping deployment".into()
                }
            },
        )?;

        if !changes_detected && !options.force {
            continue;
        }
        if let Err(error) = deploy_host(host, builder, transport, options, reporter) {
            reporter.emit(
                host.name.as_str(),
                DeployEvent::failed(DeployStep::Deploy, error.to_string()),
            )?;
            return Err(anyhow!("{}: {error}", host.name));
        }
    }
    Ok(())
}

pub(super) fn rollback<R: DeploymentReporter>(
    fleet: &Fleet,
    scope: DeploymentScope<'_>,
    builder: &dyn Builder,
    transport: &dyn Transport,
    options: DeploymentOptions,
    reporter: &mut R,
) -> Result<()> {
    let DeploymentScope::Host(host_name) = scope else {
        bail!("rollback requires a single host scope");
    };
    let host = fleet
        .hosts
        .iter()
        .find(|host| &host.name == host_name)
        .ok_or_else(|| anyhow!("host not found: {host_name}"))?;
    let mut lock = acquire_host_lock(transport, host, reporter)?;
    let result = rollback_host(host, builder, transport, options, reporter);
    finish_with_lock_release(result, &mut lock)
}

fn deploy_host<R: DeploymentReporter>(
    host: &Host,
    builder: &dyn Builder,
    transport: &dyn Transport,
    options: DeploymentOptions,
    reporter: &mut R,
) -> Result<()> {
    let build = reporter.report_step(
        host.name.as_str(),
        DeployStep::Building,
        |log| {
            builder
                .build(host, log)
                .with_context(|| format!("{}: building host configuration", host.name))
        },
        ToString::to_string,
    )?;
    let mut lock = acquire_host_lock(transport, host, reporter)?;
    let result = deploy_locked(host, builder, transport, &build, options, reporter);
    finish_with_lock_release(result, &mut lock)
}

fn deploy_locked<R: DeploymentReporter>(
    host: &Host,
    builder: &dyn Builder,
    transport: &dyn Transport,
    build: &SystemClosure,
    options: DeploymentOptions,
    reporter: &mut R,
) -> Result<()> {
    let previous = active_system_closure(transport, host)
        .with_context(|| format!("{}: reading active system closure", host.name))?;
    reporter.emit(
        host.name.as_str(),
        DeployEvent::log(
            DeployStep::Diffing,
            format!("active closure before deployment: {previous}"),
        ),
    )?;
    let target_probe_runner = copy_deployment_closures(host, builder, build, reporter)?;

    if let Err(error) = reporter.report_step(
        host.name.as_str(),
        DeployStep::Activating,
        |log| {
            activate_and_confirm(
                transport,
                host,
                build,
                &previous,
                options.active_closure_timeout,
                log,
            )
        },
        Clone::clone,
    ) {
        bail!("activation outcome was not confirmed: {error}");
    }

    if let Err(error) = reporter.report_step(
        host.name.as_str(),
        DeployStep::Checking,
        |log| {
            let current = wait_for_active_system_closure(
                transport,
                host,
                build,
                options.active_closure_timeout,
            )?;
            run_health_checks(
                transport,
                host,
                &host.health_checks,
                target_probe_runner.as_deref(),
                log,
            )?;
            Ok(current)
        },
        ToString::to_string,
    ) {
        rollback_after_failure(
            transport,
            host,
            &previous,
            target_probe_runner.as_deref(),
            options,
            error.to_string(),
            reporter,
        )?;
        return Err(error);
    }

    reporter.report_step(
        host.name.as_str(),
        DeployStep::Finalizing,
        |_| Ok("health checks verified".into()),
        Clone::clone,
    )?;
    Ok(())
}

fn rollback_host<R: DeploymentReporter>(
    host: &Host,
    builder: &dyn Builder,
    transport: &dyn Transport,
    options: DeploymentOptions,
    reporter: &mut R,
) -> Result<()> {
    let before = active_system_closure(transport, host).with_context(|| {
        format!(
            "{}: reading active system closure before rollback",
            host.name
        )
    })?;
    reporter.emit(
        host.name.as_str(),
        DeployEvent::log(
            DeployStep::RollingBack,
            format!("active closure before rollback: {before}"),
        ),
    )?;
    let target_probe_runner = copy_probe_runner_for_rollback(host, builder, reporter)?;
    reporter.report_step(
        host.name.as_str(),
        DeployStep::RollingBack,
        |_| {
            run_remote(transport, host, &rollback_command(&host.target.user))
                .with_context(|| format!("{}: rolling back", host.name))?;
            wait_for_active_system_closure_change(
                transport,
                host,
                &before,
                options.active_closure_timeout,
            )
            .with_context(|| format!("{}: verifying rollback", host.name))
        },
        ToString::to_string,
    )?;
    verify_rollback_health(host, transport, target_probe_runner.as_deref(), reporter)?;
    Ok(())
}

fn copy_deployment_closures<R: DeploymentReporter>(
    host: &Host,
    builder: &dyn Builder,
    build: &SystemClosure,
    reporter: &mut R,
) -> Result<Option<String>> {
    reporter.report_step(
        host.name.as_str(),
        DeployStep::Copying,
        |log| {
            builder
                .copy_to(build, host)
                .with_context(|| format!("{}: copying build", host.name))?;
            if requires_target_probe_runner(&host.health_checks) {
                log("copying target probe runner");
                builder
                    .copy_probe_runner(host, log)
                    .with_context(|| format!("{}: copying target probe runner", host.name))
                    .map(Some)
            } else {
                Ok(None)
            }
        },
        |runner| runner.clone().unwrap_or_else(|| "build copied".into()),
    )
}

fn copy_probe_runner_for_rollback<R: DeploymentReporter>(
    host: &Host,
    builder: &dyn Builder,
    reporter: &mut R,
) -> Result<Option<String>> {
    if !requires_target_probe_runner(&host.health_checks) {
        return Ok(None);
    }
    reporter.report_step(
        host.name.as_str(),
        DeployStep::Copying,
        |log| {
            log("copying target probe runner");
            builder
                .copy_probe_runner(host, log)
                .with_context(|| format!("{}: copying target probe runner", host.name))
                .map(Some)
        },
        |runner| {
            runner
                .clone()
                .unwrap_or_else(|| "target probe runner copied".into())
        },
    )
}

fn activate_and_confirm(
    transport: &dyn Transport,
    host: &Host,
    build: &SystemClosure,
    previous: &SystemClosure,
    active_closure_timeout: Duration,
    on_log: &mut dyn FnMut(&str),
) -> Result<String> {
    match run_remote(
        transport,
        host,
        &activation_command(&host.target.user, build, previous),
    ) {
        Ok(()) => on_log("activation command accepted; waiting for the new system"),
        Err(error) => on_log(&format!(
            "activation connection failed: {error:#}; reconciling the target before deciding the outcome"
        )),
    }
    wait_for_active_system_closure(transport, host, build, active_closure_timeout)
        .map(|closure| closure.to_string())
        .with_context(|| format!("{}: confirming activation", host.name))
}

fn rollback_after_failure<R: DeploymentReporter>(
    transport: &dyn Transport,
    host: &Host,
    previous: &SystemClosure,
    target_probe_runner: Option<&str>,
    options: DeploymentOptions,
    cause: String,
    reporter: &mut R,
) -> Result<()> {
    reporter.emit(
        host.name.as_str(),
        DeployEvent::log(
            DeployStep::RollingBack,
            format!("rolling back after failure: {cause}"),
        ),
    )?;
    reporter.report_step(
        host.name.as_str(),
        DeployStep::RollingBack,
        |_| {
            run_remote(transport, host, &rollback_command(&host.target.user))
                .with_context(|| format!("{}: rolling back after failure", host.name))?;
            wait_for_active_system_closure(
                transport,
                host,
                previous,
                options.active_closure_timeout,
            )
            .with_context(|| format!("{}: verifying rollback after failure", host.name))
        },
        ToString::to_string,
    )?;
    verify_rollback_health(host, transport, target_probe_runner, reporter)
        .context("rollback restored the previous closure but its health checks failed")?;
    Ok(())
}

fn verify_rollback_health<R: DeploymentReporter>(
    host: &Host,
    transport: &dyn Transport,
    target_probe_runner: Option<&str>,
    reporter: &mut R,
) -> Result<()> {
    reporter.report_step(
        host.name.as_str(),
        DeployStep::VerifyingRollback,
        |log| {
            run_health_checks(
                transport,
                host,
                &host.health_checks,
                target_probe_runner,
                log,
            )?;
            Ok("rollback health checks verified".to_owned())
        },
        Clone::clone,
    )?;
    Ok(())
}

fn run_remote(transport: &dyn Transport, host: &Host, command: &str) -> Result<()> {
    let destination = transport.destination(host)?;
    let output = transport
        .run(host, command)
        .with_context(|| format!("running remote command on {destination}: {command}"))?;
    ensure_success(output).map(|_| ())
}

fn ensure_success(output: CommandOutput) -> Result<String> {
    if output.status != 0 {
        let detail = if output.stderr.is_empty() {
            format!("remote command exited with status {}", output.status)
        } else {
            output.stderr
        };
        bail!(detail);
    }
    Ok(output.stdout)
}

fn wait_for_active_system_closure(
    transport: &dyn Transport,
    host: &Host,
    expected: &SystemClosure,
    timeout: Duration,
) -> Result<SystemClosure> {
    let mut last_error = anyhow!("the expected system closure was not activated");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match active_system_closure(transport, host) {
            Ok(current) if current.as_str() == expected.as_str() => return Ok(current),
            Ok(current) => {
                last_error = anyhow!("active system closure is {current}, expected {expected}")
            }
            Err(error) => last_error = error,
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1).min(remaining));
    }
    Err(last_error).with_context(|| format!("waiting for active system closure {expected}"))
}

fn wait_for_active_system_closure_change(
    transport: &dyn Transport,
    host: &Host,
    previous: &SystemClosure,
    timeout: Duration,
) -> Result<SystemClosure> {
    let mut last_error = anyhow!("the rollback did not change the active system closure");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match active_system_closure(transport, host) {
            Ok(current) if current.as_str() != previous.as_str() => return Ok(current),
            Ok(_) => last_error = anyhow!("the active system closure is still {previous}"),
            Err(error) => last_error = error,
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1).min(remaining));
    }
    Err(last_error)
        .with_context(|| format!("waiting for active system closure to change from {previous}"))
}

struct HostLock<'a> {
    transport: &'a dyn Transport,
    host: &'a Host,
    token: String,
    held: bool,
}

impl HostLock<'_> {
    fn release(&mut self) -> Result<()> {
        if !self.held {
            return Ok(());
        }
        run_remote(
            self.transport,
            self.host,
            &release_lock_command(&self.host.target.user, &self.token),
        )
        .with_context(|| format!("{}: releasing deployment lock", self.host.name))?;
        self.held = false;
        Ok(())
    }
}

impl Drop for HostLock<'_> {
    fn drop(&mut self) {
        // Best effort for early returns and unwinding. Explicit release reports
        // errors to the caller; this path cannot return one.
        let _ = self.release();
    }
}

fn acquire_host_lock<'a, R: DeploymentReporter>(
    transport: &'a dyn Transport,
    host: &'a Host,
    reporter: &mut R,
) -> Result<HostLock<'a>> {
    let token = format!(
        "pid={}; started-unix-ms={}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    reporter.report_step(
        host.name.as_str(),
        DeployStep::Locking,
        |_| {
            run_remote(
                transport,
                host,
                &acquire_lock_command(&host.target.user, &token),
            )
            .with_context(|| format!("{}: acquiring deployment lock", host.name))?;
            Ok(())
        },
        |_| "exclusive deployment lock acquired".into(),
    )?;
    Ok(HostLock {
        transport,
        host,
        token,
        held: true,
    })
}

fn finish_with_lock_release<T>(result: Result<T>, lock: &mut HostLock<'_>) -> Result<T> {
    let release = lock.release();
    match (result, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(error.context(format!(
            "also failed to release deployment lock: {release_error:#}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostName, RolloutOrder, Target};
    use std::cell::Cell;

    struct TestReporter;

    impl DeploymentReporter for TestReporter {
        fn emit(&mut self, _: &str, _: DeployEvent) -> Result<()> {
            Ok(())
        }
    }

    struct ReconnectingTransport {
        activation_attempted: Cell<bool>,
        active: SystemClosure,
    }

    impl Transport for ReconnectingTransport {
        fn run(&self, _: &Host, command: &str) -> Result<CommandOutput> {
            if command.contains("nohup sh -c") {
                self.activation_attempted.set(true);
                return Err(anyhow!("SSH connection closed during switch"));
            }
            if command == "readlink /run/current-system" {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: format!("{}\n", self.active),
                    stderr: String::new(),
                });
            }
            panic!("unexpected remote command: {command}");
        }

        fn run_with_input(&self, _: &Host, _: &str, _: &[u8]) -> Result<CommandOutput> {
            unreachable!("the reconciliation path does not run target probes")
        }

        fn destination(&self, _: &Host) -> Result<String> {
            Ok("root@app".into())
        }
    }

    struct LockHeldTransport;

    impl Transport for LockHeldTransport {
        fn run(&self, _: &Host, _: &str) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 75,
                stdout: String::new(),
                stderr: "cattix deployment lock is held by another-run".into(),
            })
        }

        fn run_with_input(&self, _: &Host, _: &str, _: &[u8]) -> Result<CommandOutput> {
            unreachable!("acquiring a lock has no input")
        }

        fn destination(&self, _: &Host) -> Result<String> {
            Ok("root@app".into())
        }
    }

    struct SameClosureTransport {
        active: SystemClosure,
    }

    impl Transport for SameClosureTransport {
        fn run(&self, _: &Host, command: &str) -> Result<CommandOutput> {
            assert_eq!(command, "readlink /run/current-system");
            Ok(CommandOutput {
                status: 0,
                stdout: format!("{}\n", self.active),
                stderr: String::new(),
            })
        }

        fn run_with_input(&self, _: &Host, _: &str, _: &[u8]) -> Result<CommandOutput> {
            unreachable!("a no-change deployment does not run target probes")
        }

        fn destination(&self, _: &Host) -> Result<String> {
            Ok("root@app".into())
        }
    }

    struct FailingBuilder {
        build_count: Cell<usize>,
    }

    impl Builder for FailingBuilder {
        fn build(&self, _: &Host, _: &mut dyn FnMut(&str)) -> Result<SystemClosure> {
            self.build_count.set(self.build_count.get() + 1);
            bail!("forced build reached")
        }

        fn copy_to(&self, _: &SystemClosure, _: &Host) -> Result<()> {
            unreachable!("the test builder fails before copying")
        }

        fn copy_probe_runner(&self, _: &Host, _: &mut dyn FnMut(&str)) -> Result<String> {
            unreachable!("the test builder fails before copying")
        }
    }

    fn host() -> Host {
        Host {
            name: HostName::from("app"),
            group: None,
            order: RolloutOrder::new(1),
            target: Target {
                host: Some("app".into()),
                port: 22,
                user: "root".into(),
            },
            metadata: Default::default(),
            extensions: Default::default(),
            health_checks: vec![],
            desired_system_closure: SystemClosure::new("/nix/store/next"),
            configuration_revision: None,
        }
    }

    #[test]
    fn reconciles_when_ssh_disconnects_during_activation() {
        let host = host();
        let transport = ReconnectingTransport {
            activation_attempted: Cell::new(false),
            active: host.desired_system_closure.clone(),
        };
        let mut log = Vec::new();
        let result = activate_and_confirm(
            &transport,
            &host,
            &host.desired_system_closure,
            &SystemClosure::new("/nix/store/previous"),
            Duration::from_secs(1),
            &mut |line| log.push(line.to_owned()),
        );
        assert_eq!(result.unwrap(), "/nix/store/next");
        assert!(transport.activation_attempted.get());
        assert!(log.iter().any(|line| line.contains("reconciling")));
    }

    #[test]
    fn lock_conflict_reports_its_existing_owner() {
        let mut reporter = TestReporter;
        let error = acquire_host_lock(&LockHeldTransport, &host(), &mut reporter)
            .err()
            .expect("a held deployment lock must reject a concurrent run");
        assert!(format!("{error:#}").contains("held by another-run"));
    }

    #[test]
    fn force_deploys_even_when_active_closure_matches() {
        let host = host();
        let fleet = Fleet {
            version: 1,
            hosts: vec![host.clone()],
            groups: Default::default(),
        };
        let transport = SameClosureTransport {
            active: host.desired_system_closure.clone(),
        };
        let builder = FailingBuilder {
            build_count: Cell::new(0),
        };
        let mut reporter = TestReporter;

        let skipped = deploy(
            &fleet,
            DeploymentScope::All,
            &builder,
            &transport,
            DeploymentOptions {
                active_closure_timeout: Duration::from_secs(1),
                force: false,
            },
            &mut reporter,
        );
        assert!(skipped.is_ok());
        assert_eq!(builder.build_count.get(), 0);

        let forced = deploy(
            &fleet,
            DeploymentScope::All,
            &builder,
            &transport,
            DeploymentOptions {
                active_closure_timeout: Duration::from_secs(1),
                force: true,
            },
            &mut reporter,
        );
        assert!(forced.is_err());
        assert_eq!(builder.build_count.get(), 1);
    }
}
