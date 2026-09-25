use anyhow::{Context, Result};
use cattix_core::{DeployEvent, DeploymentOptions, DeploymentReporter, DeploymentScope, Fleet};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) struct RunRecorder {
    file: File,
    path: PathBuf,
    run_id: String,
}

impl RunRecorder {
    pub(super) fn start(
        report_dir: Option<&Path>,
        command: &str,
        flake: &str,
        fleet: &Fleet,
        scope: DeploymentScope<'_>,
        options: DeploymentOptions,
    ) -> Result<Self> {
        let started_unix_ms = unix_millis();
        let run_id = format!("{started_unix_ms}-{}", std::process::id());
        let directory = report_dir.map_or_else(default_report_dir, Path::to_path_buf);
        fs::create_dir_all(&directory)
            .with_context(|| format!("creating Cattix report directory {}", directory.display()))?;
        let path = directory.join(format!("{run_id}.jsonl"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating Cattix run record {}", path.display()))?;
        let mut recorder = Self { file, path, run_id };
        let record_run_id = recorder.run_id.clone();
        let hosts = fleet
            .selected_hosts(scope)?
            .map(|host| RunHost {
                name: host.name.as_str(),
                desired_system_closure: host.desired_system_closure.as_str(),
            })
            .collect::<Vec<_>>();
        recorder.write(&RunStarted {
            record_type: "run-started",
            run_id: &record_run_id,
            timestamp_unix_ms: started_unix_ms,
            command,
            flake,
            active_closure_timeout_seconds: options.active_closure_timeout.as_secs(),
            hosts,
        })?;
        Ok(recorder)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn event(&mut self, host: &str, event: &DeployEvent) -> Result<()> {
        let run_id = self.run_id.clone();
        self.write(&RunEvent {
            record_type: "event",
            run_id: &run_id,
            timestamp_unix_ms: unix_millis(),
            host,
            event,
        })
    }

    pub(super) fn finish(&mut self, result: &Result<()>) -> Result<()> {
        self.write(&RunFinished {
            record_type: "run-finished",
            run_id: self.run_id.clone(),
            timestamp_unix_ms: unix_millis(),
            outcome: if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            },
            error: result.as_ref().err().map(ToString::to_string),
        })
    }

    fn write(&mut self, record: &impl Serialize) -> Result<()> {
        serde_json::to_writer(&mut self.file, record).context("encoding Cattix run record")?;
        self.file
            .write_all(b"\n")
            .context("writing Cattix run record")?;
        self.file.sync_data().context("syncing Cattix run record")
    }
}

pub(super) struct RecordingReporter<'a, R> {
    inner: R,
    recorder: &'a mut RunRecorder,
}

impl<'a, R> RecordingReporter<'a, R> {
    pub(super) fn new(inner: R, recorder: &'a mut RunRecorder) -> Self {
        Self { inner, recorder }
    }
}

impl<R: DeploymentReporter> DeploymentReporter for RecordingReporter<'_, R> {
    fn emit(&mut self, host: &str, event: DeployEvent) -> Result<()> {
        self.recorder.event(host, &event)?;
        self.inner.emit(host, event)
    }
}

#[derive(Serialize)]
struct RunHost<'a> {
    name: &'a str,
    desired_system_closure: &'a str,
}

#[derive(Serialize)]
struct RunStarted<'a> {
    #[serde(rename = "type")]
    record_type: &'static str,
    run_id: &'a str,
    timestamp_unix_ms: u128,
    command: &'a str,
    flake: &'a str,
    active_closure_timeout_seconds: u64,
    hosts: Vec<RunHost<'a>>,
}

#[derive(Serialize)]
struct RunEvent<'a> {
    #[serde(rename = "type")]
    record_type: &'static str,
    run_id: &'a str,
    timestamp_unix_ms: u128,
    host: &'a str,
    event: &'a DeployEvent,
}

#[derive(Serialize)]
struct RunFinished {
    #[serde(rename = "type")]
    record_type: &'static str,
    run_id: String,
    timestamp_unix_ms: u128,
    outcome: &'static str,
    error: Option<String>,
}

fn default_report_dir() -> PathBuf {
    if let Some(directory) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(directory).join("cattix/runs");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".local/state/cattix/runs")
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cattix_core::{
        DeploymentScope, Fleet, Host, HostName, RolloutOrder, SystemClosure, Target,
    };

    #[test]
    fn writes_a_fsynced_jsonl_lifecycle() {
        let directory =
            std::env::temp_dir().join(format!("cattix-run-record-test-{}", std::process::id()));
        let fleet = Fleet {
            version: 1,
            hosts: vec![Host {
                name: HostName::from("app"),
                group: None,
                order: RolloutOrder::new(1),
                target: Target::default(),
                metadata: Default::default(),
                extensions: Default::default(),
                health_checks: vec![],
                desired_system_closure: SystemClosure::new("/nix/store/expected"),
                configuration_revision: None,
            }],
            groups: Default::default(),
        };
        let mut recorder = RunRecorder::start(
            Some(&directory),
            "deploy",
            ".",
            &fleet,
            DeploymentScope::All,
            DeploymentOptions::default(),
        )
        .unwrap();
        recorder
            .event(
                "app",
                &DeployEvent::Started {
                    step: cattix_core::DeployStep::Building,
                },
            )
            .unwrap();
        recorder.finish(&Ok(())).unwrap();
        let record = fs::read_to_string(recorder.path()).unwrap();
        assert!(record.contains("run-started"));
        assert!(record.contains("run-finished"));
        fs::remove_dir_all(directory).unwrap();
    }
}
