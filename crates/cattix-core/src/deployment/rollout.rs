use super::{
    adapters::{Builder, CommandOutput, Transport},
    commands::{activation_command, rollback_command},
    health::{requires_target_probe_runner, run_health_checks},
    DeployEvent, DeployStep, DeploymentReporter, DeploymentScope, SystemClosureDiff,
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
                } else {
                    "no changes; skipping deployment".into()
                }
            },
        )?;

        if !changes_detected {
            continue;
        }
        if let Err(error) = deploy_host(host, builder, transport, reporter) {
            reporter.emit(
                host.name.as_str(),
                DeployEvent::failed(DeployStep::Deploy, error.to_string()),
            );
            return Err(anyhow!("{}: {error}", host.name));
        }
    }
    Ok(())
}

pub(super) fn rollback<R: DeploymentReporter>(
    fleet: &Fleet,
    scope: DeploymentScope<'_>,
    transport: &dyn Transport,
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
    let before = active_system_closure(transport, host).with_context(|| {
        format!(
            "{}: reading active system closure before rollback",
            host.name
        )
    })?;
    reporter.report_step(
        host.name.as_str(),
        DeployStep::RollingBack,
        |_| {
            run_remote(transport, host, &rollback_command(&host.target.user))
                .with_context(|| format!("{}: rolling back", host.name))?;
            wait_for_active_system_closure_change(transport, host, &before)
                .with_context(|| format!("{}: verifying rollback", host.name))
        },
        ToString::to_string,
    )?;
    Ok(())
}

fn deploy_host<R: DeploymentReporter>(
    host: &Host,
    builder: &dyn Builder,
    transport: &dyn Transport,
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
    let previous = active_system_closure(transport, host)
        .with_context(|| format!("{}: reading active system closure", host.name))?;
    let target_probe_runner = reporter.report_step(
        host.name.as_str(),
        DeployStep::Copying,
        |log| {
            builder
                .copy_to(&build, host)
                .with_context(|| format!("{}: copying build", host.name))?;
            if requires_target_probe_runner(&host.health_checks) {
                log("copying target probe runner");
                builder
                    .copy_probe_runner(host, log)
                    .with_context(|| format!("{}: copying target probe runner", host.name))
            } else {
                Ok(String::new())
            }
        },
        Clone::clone,
    )?;

    if let Err(error) = reporter.report_step(
        host.name.as_str(),
        DeployStep::Activating,
        |log| activate_and_confirm(transport, host, &build, &previous, log),
        Clone::clone,
    ) {
        bail!("activation outcome was not confirmed: {error}");
    }

    if let Err(error) = reporter.report_step(
        host.name.as_str(),
        DeployStep::Checking,
        |log| {
            let current = wait_for_active_system_closure(transport, host, &build)?;
            let target_probe_runner =
                (!target_probe_runner.is_empty()).then_some(target_probe_runner.as_str());
            run_health_checks(
                transport,
                host,
                &host.health_checks,
                target_probe_runner,
                log,
            )?;
            Ok(current)
        },
        ToString::to_string,
    ) {
        rollback_after_failure(transport, host, &previous, error.to_string(), reporter)?;
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

fn activate_and_confirm(
    transport: &dyn Transport,
    host: &Host,
    build: &SystemClosure,
    previous: &SystemClosure,
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
    wait_for_active_system_closure(transport, host, build)
        .map(|closure| closure.to_string())
        .with_context(|| format!("{}: confirming activation", host.name))
}

fn rollback_after_failure<R: DeploymentReporter>(
    transport: &dyn Transport,
    host: &Host,
    previous: &SystemClosure,
    cause: String,
    reporter: &mut R,
) -> Result<()> {
    reporter.emit(
        host.name.as_str(),
        DeployEvent::log(
            DeployStep::RollingBack,
            format!("rolling back after failure: {cause}"),
        ),
    );
    reporter.report_step(
        host.name.as_str(),
        DeployStep::RollingBack,
        |_| {
            run_remote(transport, host, &rollback_command(&host.target.user))
                .with_context(|| format!("{}: rolling back after failure", host.name))?;
            wait_for_active_system_closure(transport, host, previous)
                .with_context(|| format!("{}: verifying rollback after failure", host.name))
        },
        ToString::to_string,
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
) -> Result<SystemClosure> {
    let mut last_error = anyhow!("the expected system closure was not activated");
    for attempt in 0..60 {
        match active_system_closure(transport, host) {
            Ok(current) if current.as_str() == expected.as_str() => return Ok(current),
            Ok(current) => {
                last_error = anyhow!("active system closure is {current}, expected {expected}")
            }
            Err(error) => last_error = error,
        }
        if attempt < 59 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    Err(last_error).with_context(|| format!("waiting for active system closure {expected}"))
}

fn wait_for_active_system_closure_change(
    transport: &dyn Transport,
    host: &Host,
    previous: &SystemClosure,
) -> Result<SystemClosure> {
    let mut last_error = anyhow!("the rollback did not change the active system closure");
    for attempt in 0..60 {
        match active_system_closure(transport, host) {
            Ok(current) if current.as_str() != previous.as_str() => return Ok(current),
            Ok(_) => last_error = anyhow!("the active system closure is still {previous}"),
            Err(error) => last_error = error,
        }
        if attempt < 59 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    Err(last_error)
        .with_context(|| format!("waiting for active system closure to change from {previous}"))
}
