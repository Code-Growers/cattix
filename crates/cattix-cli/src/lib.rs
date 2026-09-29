mod deploy_tui;
mod run_record;

use anyhow::{bail, Context, Result};
use cattix_core::{
    DeployEvent, DeployStep, DeploymentOptions, DeploymentReporter, DeploymentScope, FlakeRef,
    Fleet, FleetService, GroupName, HostDiff, HostName, SystemClosure,
};
use clap::{Args, Parser, Subcommand};
use deploy_tui::{DeploymentTui, StatusColor};
use run_record::{RecordingReporter, RunRecorder};
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "cattix", about = "Fleet management for NixOS hosts")]
pub struct Cli {
    #[arg(long, global = true, default_value = ".", help = "Flake to inspect")]
    flake: String,

    #[arg(long, global = true, help = "Output machine-readable JSON")]
    json: bool,

    #[arg(
        long,
        global = true,
        help = "Allow impure Nix evaluation for controlled local fixtures"
    )]
    impure: bool,

    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help = "Directory for durable deployment JSONL records (defaults to $XDG_STATE_HOME/cattix/runs)"
    )]
    report_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Groups,
    Status {
        #[command(flatten)]
        scope: FleetScope,
    },
    Diff {
        #[command(flatten)]
        scope: FleetScope,
    },
    Plan {
        #[command(flatten)]
        scope: FleetScope,
    },
    Deploy {
        #[command(flatten)]
        scope: FleetScope,
        #[arg(long, help = "Preview closure changes without changing managed hosts")]
        dry_run: bool,
        #[arg(long, help = "Deploy even when the active and expected closures match")]
        force: bool,
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "20m",
            value_parser = parse_duration,
            help = "How long to wait for the activated or rolled-back closure (for example 45m)"
        )]
        active_closure_timeout: Duration,
    },
    Rollback {
        #[arg(long)]
        host: String,
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "20m",
            value_parser = parse_duration,
            help = "How long to wait for the rolled-back closure (for example 45m)"
        )]
        active_closure_timeout: Duration,
    },
    Update {
        #[command(flatten)]
        scope: FleetScope,
        #[arg(long)]
        input: Option<String>,
        #[arg(long)]
        mr: bool,
    },
    Scan {
        #[command(flatten)]
        scope: FleetScope,
        #[arg(
            long,
            value_name = "DIR",
            help = "Parent directory for generated SBOM and vulnerability artifacts"
        )]
        output_dir: Option<PathBuf>,
    },
    Inventory {
        #[command(flatten)]
        scope: FleetScope,
        #[arg(long)]
        sync: Option<String>,
    },
    Serve,
}

/// Selects a host, rollout group, or the complete fleet when left empty.
#[derive(Debug, Args)]
struct FleetScope {
    #[arg(long, conflicts_with = "group", help = "Select one host")]
    host: Option<String>,

    #[arg(long, conflicts_with = "host", help = "Select one rollout group")]
    group: Option<String>,
}

pub fn run() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_logging(cli.json);
    match execute(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "command failed");
            std::process::ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Groups => groups(&cli.flake, cli.json, cli.impure),
        Command::Status { scope } => status(&cli.flake, scope, cli.json, cli.impure),
        Command::Diff { scope } => diff(&cli.flake, scope, cli.json, cli.impure),
        Command::Plan { scope } => plan(&cli.flake, scope, cli.json, cli.impure),
        Command::Deploy {
            scope,
            dry_run,
            force,
            active_closure_timeout,
        } => {
            let options = DeploymentOptions {
                active_closure_timeout,
                force,
            };
            if dry_run {
                dry_run_deploy(&cli.flake, scope, cli.json, cli.impure, force)
            } else {
                deploy(
                    &cli.flake,
                    scope,
                    cli.json,
                    cli.impure,
                    cli.report_dir.as_deref(),
                    options,
                )
            }
        }
        Command::Rollback {
            host,
            active_closure_timeout,
        } => rollback(
            &cli.flake,
            &host,
            cli.json,
            cli.impure,
            cli.report_dir.as_deref(),
            DeploymentOptions {
                active_closure_timeout,
                force: false,
            },
        ),
        Command::Scan { scope, output_dir } => scan(
            &cli.flake,
            scope,
            cli.json,
            cli.impure,
            output_dir.as_deref(),
        ),
        Command::Update { .. } | Command::Inventory { .. } | Command::Serve => {
            bail!("this command is not implemented yet")
        }
    }
}

fn init_logging(json: bool) {
    let filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("cattix=info"));

    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter())
            .json()
            .flatten_event(true)
            .with_target(false)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter())
            .compact()
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }
}

fn load_fleet_config(flake: &str, impure: bool) -> Result<Fleet> {
    let flake = FlakeRef::from(flake);
    FleetService::default()
        .load_fleet(&flake, impure)
        .with_context(|| format!("loading fleet from flake {flake}"))
}

fn write_json(value: &impl serde::Serialize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer_pretty(&mut stdout, value).context("encoding JSON output")?;
    stdout.write_all(b"\n").context("writing JSON output")?;
    Ok(())
}

fn groups(flake: &str, json: bool, impure: bool) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let groups = fleet.rollout_groups();

    if json {
        write_json(&groups)?;
    } else if groups.is_empty() {
        tracing::info!("no groups configured");
    } else {
        for group in groups {
            tracing::info!(
                group = %group.name,
                hosts = %group.hosts.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
                "rollout group"
            );
        }
    }
    Ok(())
}

fn scan(
    flake: &str,
    selection: FleetScope,
    json: bool,
    impure: bool,
    output_dir: Option<&std::path::Path>,
) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let host_name = selection.host.as_deref().map(HostName::from);
    let group = selection.group.as_deref().map(GroupName::from);

    let base_dir = output_dir
        .map(PathBuf::from)
        .unwrap_or_else(default_scan_dir);
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_nanos();
    let run_dir = base_dir.join(format!("{run_id}-{}", std::process::id()));
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("creating scan directory {}", run_dir.display()))?;

    let reports = FleetService::default().scan(
        &fleet,
        deployment_scope(host_name.as_ref(), group.as_ref()),
        &FlakeRef::from(flake),
        impure,
        &run_dir,
    )?;
    let manifest = run_dir.join("report.json");
    std::fs::write(&manifest, serde_json::to_vec_pretty(&reports)?)
        .with_context(|| format!("writing scan manifest {}", manifest.display()))?;

    if json {
        write_json(&serde_json::json!({
            "report": manifest,
            "hosts": reports,
        }))?;
    } else if reports.is_empty() {
        tracing::info!(directory = %run_dir.display(), "no hosts matched");
    } else {
        for report in &reports {
            tracing::info!(
                host = %report.host,
                active_closure = %report.active_system_closure,
                desired_closure = %report.desired_system_closure,
                active_artifacts = %report.active.directory.display(),
                desired_artifacts = %report.desired.directory.display(),
                "vulnerability scan complete"
            );
        }
        tracing::info!(report = %manifest.display(), "scan report manifest");
    }
    Ok(())
}

fn default_scan_dir() -> PathBuf {
    if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(state_home).join("cattix/scans");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/cattix/scans");
    }
    PathBuf::from(".cattix/scans")
}

fn status(flake: &str, selection: FleetScope, json: bool, impure: bool) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let host_name = selection.host.as_deref().map(HostName::from);
    let group = selection.group.as_deref().map(GroupName::from);
    let results = FleetService::default()
        .status(&fleet, deployment_scope(host_name.as_ref(), group.as_ref()))?;

    if json {
        write_json(&results)?;
    } else {
        for result in &results {
            tracing::info!(
                host = %result.host,
                state = result.state.as_str(),
                active_system_closure = result
                    .active_system_closure
                    .as_ref()
                    .map(SystemClosure::as_str)
                    .unwrap_or("-"),
                "host status"
            );
            if let Some(error) = &result.error {
                tracing::warn!(host = %result.host, error = %error, "host is unreachable");
            }
        }
    }

    Ok(())
}
fn diff_as_json(diff: &HostDiff) -> serde_json::Value {
    match &diff.report {
        Some(report) => serde_json::json!({
            "host": diff.host,
            "deployed": diff.deployed,
            "expected": diff.expected,
            "report": report,
        }),
        None => serde_json::json!({
            "host": diff.host,
            "deployed": diff.deployed,
            "expected": diff.expected,
            "changes": [],
        }),
    }
}

fn diff(flake: &str, selection: FleetScope, json: bool, impure: bool) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let has_host_selection = selection.host.is_some();
    let host_name = selection.host.as_deref().map(HostName::from);
    let group = selection.group.as_deref().map(GroupName::from);
    let reports = FleetService::default().diff(
        &fleet,
        deployment_scope(host_name.as_ref(), group.as_ref()),
        &FlakeRef::from(flake),
        impure,
    )?;

    if json {
        let output = if has_host_selection {
            diff_as_json(&reports[0])
        } else {
            serde_json::Value::Array(reports.iter().map(diff_as_json).collect())
        };
        write_json(&output)?;
    } else {
        for report in reports {
            match report.report {
                None => tracing::info!(host = %report.host, "no changes"),
                Some(report_text) => {
                    tracing::info!(host = %report.host, report = %report_text, "build diff");
                }
            }
        }
    }

    Ok(())
}

fn plan(flake: &str, selection: FleetScope, json: bool, impure: bool) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let host_name = selection.host.as_deref().map(HostName::from);
    let group = selection.group.as_deref().map(GroupName::from);
    let steps = FleetService::default()
        .plan(&fleet, deployment_scope(host_name.as_ref(), group.as_ref()))?;

    if json {
        write_json(&steps)?;
    } else if steps.is_empty() {
        tracing::info!("no hosts matched");
    } else {
        for step in &steps {
            tracing::info!(
                position = step.position,
                host = %step.host,
                group = step.group.as_ref().map(GroupName::as_str),
                order = step.order.value(),
                steps = ?step.steps,
                "deployment plan"
            );
        }
    }

    Ok(())
}

fn dry_run_deploy(
    flake: &str,
    selection: FleetScope,
    json: bool,
    impure: bool,
    force: bool,
) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let host_name = selection.host.as_deref().map(HostName::from);
    let group = selection.group.as_deref().map(GroupName::from);
    let scope = deployment_scope(host_name.as_ref(), group.as_ref());
    let service = FleetService::default();
    let plan = service.plan(&fleet, scope)?;
    let diffs = service.diff(&fleet, scope, &FlakeRef::from(flake), impure)?;
    if force {
        for (step, diff) in plan.iter().zip(&diffs) {
            if diff.report.is_none() {
                let built =
                    service.build_expected_host(&FlakeRef::from(flake), &step.host, impure)?;
                if built != diff.expected {
                    bail!(
                        "flake evaluation expected {}, but building {} produced {}",
                        diff.expected,
                        step.host,
                        built
                    );
                }
            }
        }
    }

    if json {
        let hosts = plan
            .iter()
            .zip(&diffs)
            .map(|(step, diff)| {
                serde_json::json!({
                    "position": step.position,
                    "host": step.host,
                    "group": step.group,
                    "order": step.order,
                    "steps": step.steps,
                    "would_deploy": force || diff.report.is_some(),
                    "forced": force,
                    "deployed": diff.deployed,
                    "expected": diff.expected,
                    "report": diff.report,
                })
            })
            .collect::<Vec<_>>();
        write_json(&serde_json::json!({
            "dry_run": true,
            "hosts": hosts,
        }))?;
    } else {
        tracing::info!(
            "dry run only: no managed host will be changed; desired closures may be built locally"
        );
        for (step, diff) in plan.iter().zip(&diffs) {
            tracing::info!(
                position = step.position,
                host = %step.host,
                group = step.group.as_ref().map(GroupName::as_str),
                order = step.order.value(),
                would_deploy = force || diff.report.is_some(),
                forced = force,
                deployed = %diff.deployed,
                expected = %diff.expected,
                "dry-run host"
            );
            if let Some(report) = &diff.report {
                tracing::info!(host = %diff.host, report = %report, "build diff");
            }
        }
        if plan.is_empty() {
            tracing::info!("no hosts matched");
        }
    }

    Ok(())
}

struct TracingReporter;

impl DeploymentReporter for TracingReporter {
    fn emit(&mut self, host: &str, event: DeployEvent) -> Result<()> {
        match event {
            DeployEvent::Started { step } => tracing::info!(
                host = %host,
                step = %step,
                status = "started",
                "deployment event"
            ),
            DeployEvent::Log { step, message } => tracing::info!(
                host = %host,
                step = %step,
                status = "running",
                log = %message,
                "deployment log"
            ),
            DeployEvent::Done { step, detail } => tracing::info!(
                host = %host,
                step = %step,
                status = "done",
                detail = %detail,
                "deployment event"
            ),
            DeployEvent::Failed { step, error } => tracing::info!(
                host = %host,
                step = %step,
                status = "failed",
                detail = %error,
                "deployment event"
            ),
        }
        Ok(())
    }
}

struct TerminalReporter(DeploymentTui);

impl TerminalReporter {
    fn new() -> std::io::Result<Self> {
        DeploymentTui::start().map(Self)
    }

    fn log(&self, host: &str, step: DeployStep, message: &str) -> std::io::Result<()> {
        let lines = message
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| format!("  │ [{host} · {}] {line}", step.as_str()))
            .collect();
        self.0.logs(lines)
    }

    fn operation(step: DeployStep) -> &'static str {
        match step {
            DeployStep::Diffing => "Compare active and desired system closures",
            DeployStep::Building => "Build the desired NixOS system",
            DeployStep::Locking => "Acquire the deployment lock",
            DeployStep::Copying => "Copy the system closure to the target",
            DeployStep::Activating => "Activate the new NixOS generation",
            DeployStep::Checking => "Wait for activation and run health checks",
            DeployStep::Finalizing => "Finalize the deployment",
            DeployStep::Deploy => "Deploy the host",
            DeployStep::RollingBack => "Restore the previous NixOS generation",
            DeployStep::VerifyingRollback => "Verify rollback and health checks",
        }
    }

    fn completion(step: DeployStep, detail: &str) -> String {
        match (step, detail) {
            (DeployStep::Diffing, "changes detected") => {
                "Active and desired closures differ".into()
            }
            (DeployStep::Diffing, "no changes; skipping deployment") => {
                "Closures match; host skipped".into()
            }
            (DeployStep::Diffing, "no changes; forced deployment") => {
                "Closures match; forced deployment will continue".into()
            }
            (DeployStep::Building, _) => format!("System build complete: {detail}"),
            (DeployStep::Locking, _) => "Deployment lock acquired".into(),
            (DeployStep::Copying, _) => format!("System copy complete: {detail}"),
            (DeployStep::Activating, _) => format!("Activation confirmed: {detail}"),
            (DeployStep::Checking, _) => "Activation and health checks passed".into(),
            (DeployStep::Finalizing, _) => "Host is healthy and ready".into(),
            (DeployStep::RollingBack, _) => format!("Rollback command complete: {detail}"),
            (DeployStep::VerifyingRollback, _) => "Rollback and health checks passed".into(),
            (DeployStep::Deploy, _) => format!("Deployment complete: {detail}"),
            (DeployStep::Diffing, _) => format!("Closure comparison complete: {detail}"),
        }
    }
}

impl DeploymentReporter for TerminalReporter {
    fn emit(&mut self, host: &str, event: DeployEvent) -> Result<()> {
        match event {
            DeployEvent::Started { step } => {
                self.0.status(
                    StatusColor::Cyan,
                    format!("→ {host} {}", Self::operation(step)),
                )?;
            }
            DeployEvent::Log { step, message } => self.log(host, step, &message)?,
            DeployEvent::Done { step, detail } => {
                self.0.status(
                    StatusColor::Green,
                    format!("✔ {host} {}", Self::completion(step, &detail)),
                )?;
            }
            DeployEvent::Failed { step, error } => {
                self.0.status(
                    StatusColor::Red,
                    format!("✘ {host} Failed while {}: {error}", Self::operation(step)),
                )?;
            }
        }
        Ok(())
    }
}

enum CliReporter {
    Terminal(TerminalReporter),
    Tracing(TracingReporter),
}

impl DeploymentReporter for CliReporter {
    fn emit(&mut self, host: &str, event: DeployEvent) -> Result<()> {
        match self {
            Self::Terminal(reporter) => reporter.emit(host, event),
            Self::Tracing(reporter) => reporter.emit(host, event),
        }
    }
}

fn deployment_reporter(json: bool) -> CliReporter {
    if !json && std::io::stdout().is_terminal() {
        match TerminalReporter::new() {
            Ok(reporter) => CliReporter::Terminal(reporter),
            Err(error) => {
                tracing::warn!(%error, "could not start deployment TUI; using plain event output");
                CliReporter::Tracing(TracingReporter)
            }
        }
    } else {
        CliReporter::Tracing(TracingReporter)
    }
}

fn deploy(
    flake: &str,
    selection: FleetScope,
    json: bool,
    impure: bool,
    report_dir: Option<&std::path::Path>,
    options: DeploymentOptions,
) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let flake = FlakeRef::from(flake);
    let host_name = selection.host.as_deref().map(HostName::from);
    let group = selection.group.as_deref().map(GroupName::from);
    let scope = deployment_scope(host_name.as_ref(), group.as_ref());
    let mut recorder =
        RunRecorder::start(report_dir, "deploy", flake.as_str(), &fleet, scope, options)?;
    if !json {
        tracing::info!(report = %recorder.path().display(), "writing durable deployment record");
    }
    let mut reporter = RecordingReporter::new(deployment_reporter(json), &mut recorder);
    let result =
        FleetService::default().deploy(&fleet, scope, &flake, impure, options, &mut reporter);
    drop(reporter);
    recorder.finish(&result)?;
    result
}

fn deployment_scope<'a>(
    host: Option<&'a HostName>,
    group: Option<&'a GroupName>,
) -> DeploymentScope<'a> {
    match (host, group) {
        (Some(host), None) => DeploymentScope::Host(host),
        (None, Some(group)) => DeploymentScope::Group(group),
        (None, None) => DeploymentScope::All,
        (Some(_), Some(_)) => unreachable!("clap rejects --host with --group"),
    }
}

fn rollback(
    flake: &str,
    host_name: &str,
    json: bool,
    impure: bool,
    report_dir: Option<&std::path::Path>,
    options: DeploymentOptions,
) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let host_name = HostName::from(host_name);
    let scope = DeploymentScope::Host(&host_name);
    let mut recorder = RunRecorder::start(report_dir, "rollback", flake, &fleet, scope, options)?;
    if !json {
        tracing::info!(report = %recorder.path().display(), "writing durable deployment record");
    }
    let flake = FlakeRef::from(flake);
    let mut reporter = RecordingReporter::new(deployment_reporter(json), &mut recorder);
    let result =
        FleetService::default().rollback(&fleet, scope, &flake, impure, options, &mut reporter);
    drop(reporter);
    recorder.finish(&result)?;
    result
}

fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, unit) = value.split_at(split);
    let amount = amount
        .parse::<u64>()
        .map_err(|_| "duration must start with a positive integer".to_owned())?;
    if amount == 0 {
        return Err("duration must be greater than zero".into());
    }
    let seconds = match unit {
        "s" => amount,
        "m" => amount
            .checked_mul(60)
            .ok_or_else(|| "duration is too large".to_owned())?,
        "h" => amount
            .checked_mul(60 * 60)
            .ok_or_else(|| "duration is too large".to_owned())?,
        _ => return Err("duration must use s, m, or h (for example 45m)".into()),
    };
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn fleet_commands_share_host_and_group_selection() {
        for command in [
            "status",
            "diff",
            "plan",
            "deploy",
            "scan",
            "inventory",
            "update",
        ] {
            assert!(Cli::try_parse_from(["cattix", command, "--host", "app-primary"]).is_ok());
            assert!(Cli::try_parse_from(["cattix", command, "--group", "app"]).is_ok());
            assert!(Cli::try_parse_from([
                "cattix",
                command,
                "--host",
                "app-primary",
                "--group",
                "app"
            ])
            .is_err());
        }
    }

    #[test]
    fn rollback_uses_the_standard_host_flag() {
        assert!(Cli::try_parse_from(["cattix", "rollback", "--host", "app-primary"]).is_ok());
        assert!(Cli::try_parse_from(["cattix", "rollback", "app-primary"]).is_err());
    }

    #[test]
    fn active_closure_timeout_accepts_human_scale_durations() {
        let cli =
            Cli::try_parse_from(["cattix", "deploy", "--active-closure-timeout", "45m"]).unwrap();
        assert!(
            matches!(cli.command, Command::Deploy { active_closure_timeout, .. } if active_closure_timeout == std::time::Duration::from_secs(45 * 60))
        );
        assert!(
            Cli::try_parse_from(["cattix", "deploy", "--active-closure-timeout", "0m",]).is_err()
        );
    }

    #[test]
    fn deploy_accepts_dry_run_flag() {
        let cli = Cli::try_parse_from(["cattix", "deploy", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Command::Deploy { dry_run: true, .. }));
    }

    #[test]
    fn deploy_accepts_force_flag() {
        let cli = Cli::try_parse_from(["cattix", "deploy", "--force"]).unwrap();
        assert!(matches!(cli.command, Command::Deploy { force: true, .. }));
    }
}
