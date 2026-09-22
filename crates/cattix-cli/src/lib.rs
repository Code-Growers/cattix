use anyhow::{bail, Context, Result};
use cattix_core::{
    DeployEvent, DeployStep, DeploymentReporter, DeploymentScope, FlakeRef, Fleet, FleetService,
    GroupName, HostDiff, HostName, SystemClosure,
};
use clap::{Args, Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::{
    collections::HashMap,
    io::{IsTerminal, Write},
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
    },
    Rollback {
        #[arg(long)]
        host: String,
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
        Command::Deploy { scope } => deploy(&cli.flake, scope, cli.json, cli.impure),
        Command::Rollback { host } => rollback(&cli.flake, &host, cli.json, cli.impure),
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
    let reports = FleetService::default()
        .diff(&fleet, deployment_scope(host_name.as_ref(), group.as_ref()))?;

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

struct TracingReporter;

impl DeploymentReporter for TracingReporter {
    fn emit(&mut self, host: &str, event: DeployEvent) {
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
    }
}

struct TerminalReporter {
    progress: MultiProgress,
    hosts: HashMap<String, HostTree>,
    steps: HashMap<(String, DeployStep), ProgressBar>,
    lines: Vec<ProgressBar>,
}

struct HostTree {
    tail: ProgressBar,
}

impl TerminalReporter {
    fn new() -> Self {
        Self {
            progress: MultiProgress::new(),
            hosts: HashMap::new(),
            steps: HashMap::new(),
            lines: Vec::new(),
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
            let tail = self.host_tail(host);
            let line = self
                .progress
                .insert_after(&tail, ProgressBar::new_spinner());
            line.set_style(Self::log_style());
            line.finish_with_message(message.to_owned());
            self.append(host, line.clone());
            self.lines.push(line);
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
    fn emit(&mut self, host: &str, event: DeployEvent) {
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
    }
}

enum CliReporter {
    Terminal(TerminalReporter),
    Tracing(TracingReporter),
}

impl DeploymentReporter for CliReporter {
    fn emit(&mut self, host: &str, event: DeployEvent) {
        match self {
            Self::Terminal(reporter) => reporter.emit(host, event),
            Self::Tracing(reporter) => reporter.emit(host, event),
        }
    }
}

fn deployment_reporter(json: bool) -> CliReporter {
    if !json && std::io::stderr().is_terminal() {
        CliReporter::Terminal(TerminalReporter::new())
    } else {
        CliReporter::Tracing(TracingReporter)
    }
}

fn deploy(flake: &str, selection: FleetScope, json: bool, impure: bool) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let flake = FlakeRef::from(flake);
    let host_name = selection.host.as_deref().map(HostName::from);
    let group = selection.group.as_deref().map(GroupName::from);
    let scope = deployment_scope(host_name.as_ref(), group.as_ref());
    let mut reporter = deployment_reporter(json);
    FleetService::default().deploy(&fleet, scope, &flake, impure, &mut reporter)
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

fn rollback(flake: &str, host_name: &str, json: bool, impure: bool) -> Result<()> {
    let fleet = load_fleet_config(flake, impure)?;
    let host_name = HostName::from(host_name);
    let mut reporter = deployment_reporter(json);
    FleetService::default().rollback(&fleet, DeploymentScope::Host(&host_name), &mut reporter)
}

#[cfg(test)]
mod tests {
    use super::Cli;
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
}
