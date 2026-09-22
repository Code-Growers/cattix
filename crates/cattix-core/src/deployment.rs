mod adapters;
mod commands;
mod health;
mod rollout;

use crate::{FlakeRef, Fleet, GroupName, Host, HostName, HostState, RolloutOrder, SystemClosure};
use adapters::{NixDeploymentBackend, SshTransport};
use anyhow::{Context, Result};
use cattix_nix::Nix;
use cattix_transport::OpenSsh;
use std::fmt;

pub use health::run_local_health_check;

/// Application service shared by command-line and server frontends.
///
/// It owns decoding the fleet contract and orchestrates the Nix and SSH adapters.
#[derive(Debug, Default)]
pub struct FleetService {
    nix: Nix,
    ssh: OpenSsh,
}

impl FleetService {
    pub fn load_fleet(&self, flake: &FlakeRef, impure: bool) -> Result<Fleet> {
        let document = self
            .nix
            .eval_json(flake.as_str(), "cattix", impure)
            .context("evaluating fleet definition")?;
        Fleet::from_json_slice(&document).context("decoding fleet definition")
    }

    pub fn active_system_closure(&self, host: &Host) -> Result<SystemClosure> {
        rollout::active_system_closure(&SshTransport { ssh: &self.ssh }, host)
    }

    pub fn system_closure_diff(&self, host: &Host) -> Result<SystemClosureDiff> {
        rollout::system_closure_diff(&SshTransport { ssh: &self.ssh }, host)
    }

    pub fn nvd_diff(
        &self,
        active_system_closure: &SystemClosure,
        desired_system_closure: &SystemClosure,
    ) -> Result<String> {
        self.nix.nvd_diff(
            active_system_closure.as_str(),
            desired_system_closure.as_str(),
        )
    }

    pub fn status(&self, fleet: &Fleet, scope: DeploymentScope<'_>) -> Result<Vec<HostStatus>> {
        fleet
            .selected_hosts(scope)?
            .map(|host| match self.active_system_closure(host) {
                Ok(active_system_closure) => Ok(HostStatus {
                    host: host.name.clone(),
                    state: host.state_for(Some(active_system_closure.as_str())),
                    expected_build: host.desired_system_closure.clone(),
                    active_system_closure: Some(active_system_closure),
                    error: None,
                }),
                Err(error) => Ok(HostStatus {
                    host: host.name.clone(),
                    state: HostState::Unreachable,
                    expected_build: host.desired_system_closure.clone(),
                    active_system_closure: None,
                    error: Some(error.to_string()),
                }),
            })
            .collect()
    }

    pub fn diff(&self, fleet: &Fleet, scope: DeploymentScope<'_>) -> Result<Vec<HostDiff>> {
        fleet
            .selected_hosts(scope)?
            .map(|host| {
                let diff = self.system_closure_diff(host)?;
                let report = diff
                    .has_changes()
                    .then(|| {
                        self.nix.nvd_diff(
                            diff.active_system_closure.as_str(),
                            diff.desired_system_closure.as_str(),
                        )
                    })
                    .transpose()
                    .with_context(|| format!("calculating build diff for {}", host.name))?;
                Ok(HostDiff {
                    host: host.name.clone(),
                    deployed: diff.active_system_closure,
                    expected: diff.desired_system_closure,
                    report,
                })
            })
            .collect()
    }

    pub fn plan(&self, fleet: &Fleet, scope: DeploymentScope<'_>) -> Result<Vec<DeploymentPlan>> {
        Ok(fleet
            .selected_hosts(scope)?
            .enumerate()
            .map(|(index, host)| DeploymentPlan {
                position: index + 1,
                host: host.name.clone(),
                group: host.group.clone(),
                order: host.order,
                steps: vec![
                    DeployStep::Diffing,
                    DeployStep::Building,
                    DeployStep::Copying,
                    DeployStep::Activating,
                    DeployStep::Checking,
                    DeployStep::Finalizing,
                ],
            })
            .collect())
    }

    pub fn deploy<R: DeploymentReporter>(
        &self,
        fleet: &Fleet,
        scope: DeploymentScope<'_>,
        flake: &FlakeRef,
        impure: bool,
        reporter: &mut R,
    ) -> Result<()> {
        let builder = NixDeploymentBackend {
            nix: &self.nix,
            flake,
            impure,
        };
        rollout::deploy(
            fleet,
            scope,
            &builder,
            &SshTransport { ssh: &self.ssh },
            reporter,
        )
    }

    pub fn rollback<R: DeploymentReporter>(
        &self,
        fleet: &Fleet,
        scope: DeploymentScope<'_>,
        reporter: &mut R,
    ) -> Result<()> {
        rollout::rollback(fleet, scope, &SshTransport { ssh: &self.ssh }, reporter)
    }
}

/// Stable wire response emitted by `cattix-probe-runner` on a target host.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct TargetProbeResult {
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeployStep {
    Diffing,
    Building,
    Copying,
    Activating,
    Checking,
    Finalizing,
    Deploy,
    RollingBack,
}

impl DeployStep {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diffing => "diffing",
            Self::Building => "building",
            Self::Copying => "copying",
            Self::Activating => "activating",
            Self::Checking => "checking",
            Self::Finalizing => "finalizing",
            Self::Deploy => "deploy",
            Self::RollingBack => "rolling-back",
        }
    }
}

impl fmt::Display for DeployStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub enum DeployEvent {
    Started { step: DeployStep },
    Log { step: DeployStep, message: String },
    Done { step: DeployStep, detail: String },
    Failed { step: DeployStep, error: String },
}

impl DeployEvent {
    pub(super) fn log(step: DeployStep, message: impl Into<String>) -> Self {
        Self::Log {
            step,
            message: message.into(),
        }
    }

    pub(super) fn failed(step: DeployStep, error: impl Into<String>) -> Self {
        Self::Failed {
            step,
            error: error.into(),
        }
    }
}

/// Receives state-machine events; the CLI and future server render them differently.
pub trait DeploymentReporter {
    fn emit(&mut self, host: &str, event: DeployEvent);

    fn report_step<T>(
        &mut self,
        host: &str,
        step: DeployStep,
        operation: impl FnOnce(&mut dyn FnMut(&str)) -> Result<T>,
        detail: impl FnOnce(&T) -> String,
    ) -> Result<T> {
        self.emit(host, DeployEvent::Started { step });
        let result = {
            let mut report_log = |message: &str| self.emit(host, DeployEvent::log(step, message));
            operation(&mut report_log)
        };

        match result {
            Ok(value) => {
                self.emit(
                    host,
                    DeployEvent::Done {
                        step,
                        detail: detail(&value),
                    },
                );
                Ok(value)
            }
            Err(error) => {
                self.emit(host, DeployEvent::failed(step, error.to_string()));
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DeploymentScope<'a> {
    All,
    Group(&'a GroupName),
    Host(&'a HostName),
}

#[derive(Debug)]
pub struct SystemClosureDiff {
    pub active_system_closure: SystemClosure,
    pub desired_system_closure: SystemClosure,
}

impl SystemClosureDiff {
    pub fn has_changes(&self) -> bool {
        self.active_system_closure != self.desired_system_closure
    }
}

/// Frontend-neutral status of one host.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostStatus {
    pub host: HostName,
    pub state: HostState,
    pub expected_build: SystemClosure,
    #[serde(rename = "current_build")]
    pub active_system_closure: Option<SystemClosure>,
    pub error: Option<String>,
}

/// Frontend-neutral detailed diff for one host.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostDiff {
    pub host: HostName,
    pub deployed: SystemClosure,
    pub expected: SystemClosure,
    pub report: Option<String>,
}

/// A host's position and actions in a rollout plan.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeploymentPlan {
    pub position: usize,
    pub host: HostName,
    pub group: Option<GroupName>,
    pub order: RolloutOrder,
    pub steps: Vec<DeployStep>,
}
