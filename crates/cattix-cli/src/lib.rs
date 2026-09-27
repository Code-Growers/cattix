mod run_record;

use anyhow::{bail, Context, Result};
use cattix_core::{
    DeployEvent, DeployStep, DeploymentOptions, DeploymentReporter, DeploymentScope, FlakeRef,
    Fleet, FleetService, GroupName, HostDiff, HostName, SystemClosure,
};
use clap::{Args, Parser, Subcommand};
use console::Term;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use run_record::{RecordingReporter, RunRecorder};
use std::{
    collections::{HashMap, VecDeque},
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
        Command::Update { .. }
        | Command::Scan { .. }
        | Command::Inventory { .. }
        | Command::Serve => bail!("this command is not implemented yet"),
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
            .with_writer(std::io::stdout)
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

struct TerminalReporter {
    progress: MultiProgress,
    hosts: HashMap<String, HostTree>,
    steps: HashMap<(String, DeployStep), ProgressBar>,
    max_log_lines: usize,
    visible_log_lines: VecDeque<ProgressBar>,
    suppressed_log_lines: usize,
    log_summary: Option<ProgressBar>,
}

struct HostTree {
    tail: ProgressBar,
}

impl TerminalReporter {
    fn new() -> Self {
        let rows = Term::stderr().size().0 as usize;
        Self {
            // Limit terminal refreshes so bursty child-process output can't
            // make the retained progress tree flicker.
            progress: MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8)),
            hosts: HashMap::new(),
            steps: HashMap::new(),
            max_log_lines: terminal_log_height(rows),
            visible_log_lines: VecDeque::new(),
            suppressed_log_lines: 0,
            log_summary: None,
        }
    }

    fn host_tail(&mut self, host: &str) -> ProgressBar {
        if let Some(tree) = self.hosts.get(host) {
            return tree.tail.clone();
        }

        let bar = self.progress.add(ProgressBar::new_spinner());
        bar.set_style(ProgressStyle::with_template("▾ {msg}").expect("valid host template"));
        bar.finish_with_message(host.to_owned());
        self.hosts
            .insert(host.to_owned(), HostTree { tail: bar.clone() });
        bar
    }

    fn append(&mut self, host: &str, bar: ProgressBar) {
        self.hosts
            .get_mut(host)
            .expect("host tree is created before appending rows")
            .tail = bar;
    }

    fn step(&mut self, host: &str, step: DeployStep) -> ProgressBar {
        let key = (host.to_owned(), step);
        if let Some(bar) = self.steps.get(&key) {
            return bar.clone();
        }

        let tail = self.host_tail(host);
        let bar = self
            .progress
            .insert_after(&tail, ProgressBar::new_spinner());
        bar.set_prefix(step.to_string());
        self.steps.insert(key, bar.clone());
        self.append(host, bar.clone());
        bar
    }

    fn log(&mut self, host: &str, step: DeployStep, message: &str) {
        self.step(host, step);

        for message in message.lines().filter(|line| !line.is_empty()) {
            if self.log_summary.is_some() {
                if self.max_log_lines > 1 {
                    self.hide_oldest_log_line();
                    self.add_log_line(host, step, message);
                } else {
                    self.suppressed_log_lines += 1;
                }
                self.update_log_summary();
                continue;
            }

            if self.visible_log_lines.len() < self.max_log_lines {
                self.add_log_line(host, step, message);
            } else if self.max_log_lines == 1 {
                self.hide_oldest_log_line();
                self.suppressed_log_lines += 1;
                self.create_log_summary();
            } else {
                while self.visible_log_lines.len() > self.max_log_lines - 2 {
                    self.hide_oldest_log_line();
                }
                self.add_log_line(host, step, message);
                self.create_log_summary();
            }
        }
    }

    fn add_log_line(&mut self, host: &str, step: DeployStep, message: &str) {
        let line = self.progress.add(ProgressBar::new_spinner());
        line.set_style(Self::log_style());
        line.finish_with_message(format!("{host} {step}: {message}"));
        self.visible_log_lines.push_back(line);
    }

    fn hide_oldest_log_line(&mut self) {
        if let Some(oldest) = self.visible_log_lines.pop_front() {
            self.progress.remove(&oldest);
            self.suppressed_log_lines += 1;
        }
    }

    fn create_log_summary(&mut self) {
        let summary = self.progress.add(ProgressBar::new(1));
        summary.set_style(Self::log_style());
        self.log_summary = Some(summary);
        self.update_log_summary();
    }

    fn update_log_summary(&self) {
        if let Some(summary) = &self.log_summary {
            summary.set_message(format!(
                "… {} earlier log lines hidden (showing latest output)",
                self.suppressed_log_lines
            ));
        }
    }

    fn running_style() -> ProgressStyle {
        ProgressStyle::with_template("  {spinner:.cyan} {prefix} {msg}")
            .expect("valid running-step template")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
    }

    fn done_style() -> ProgressStyle {
        ProgressStyle::with_template("  ✔ {prefix} {msg}").expect("valid done-step template")
    }

    fn failed_style() -> ProgressStyle {
        ProgressStyle::with_template("  ✘ {prefix} {msg}").expect("valid failed-step template")
    }

    fn log_style() -> ProgressStyle {
        ProgressStyle::with_template("  │   {msg}").expect("valid log template")
    }
}

impl DeploymentReporter for TerminalReporter {
    fn emit(&mut self, host: &str, event: DeployEvent) -> Result<()> {
        match event {
            DeployEvent::Started { step } => {
                let bar = self.step(host, step);
                bar.set_style(Self::running_style());
                bar.set_message("running");
                bar.enable_steady_tick(std::time::Duration::from_millis(100));
            }
            DeployEvent::Log { step, message } => {
                self.log(host, step, &message);
            }
            DeployEvent::Done { step, detail } => {
                let bar = self.step(host, step);
                bar.set_style(Self::done_style());
                bar.finish_with_message(detail);
            }
            DeployEvent::Failed { step, error } => {
                let bar = self.step(host, step);
                bar.set_style(Self::failed_style());
                bar.finish_with_message(error);
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

fn terminal_log_height(rows: usize) -> usize {
    (rows / 3).max(1)
}

fn deployment_reporter(json: bool) -> CliReporter {
    if !json && std::io::stderr().is_terminal() {
        CliReporter::Terminal(TerminalReporter::new())
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
    use super::{terminal_log_height, Cli, Command, DeployStep, TerminalReporter};
    use clap::Parser;
    use indicatif::{MultiProgress, ProgressDrawTarget};

    #[test]
    fn terminal_log_window_uses_one_third_of_the_screen() {
        assert_eq!(terminal_log_height(24), 8);
        assert_eq!(terminal_log_height(60), 20);
        assert_eq!(terminal_log_height(2), 1);
    }

    #[test]
    fn terminal_logs_are_bounded_and_keep_the_latest_lines() {
        let mut reporter = TerminalReporter::new();
        reporter.progress = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        reporter.max_log_lines = 4;

        reporter.log(
            "app",
            DeployStep::Building,
            "one\ntwo\nthree\nfour\nfive\nsix\nseven",
        );

        assert_eq!(reporter.visible_log_lines.len(), 3);
        assert_eq!(reporter.suppressed_log_lines, 4);
        assert!(reporter.log_summary.is_some());
    }

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
