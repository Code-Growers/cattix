use anyhow::{anyhow, bail, Context, Result};
use cattix_utils::string_value;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

use crate::DeploymentScope;

string_value!(
    /// Nix store path of a complete NixOS system closure.
    SystemClosure
);
string_value!(
    /// Stable identifier for a host in a fleet.
    HostName
);
string_value!(
    /// Stable identifier for a rollout group.
    GroupName
);
string_value!(
    /// Nix flake reference supplied to Cattix.
    FlakeRef
);
string_value!(
    /// Revision recorded by a NixOS configuration.
    ConfigurationRevision
);

/// Position of a host within its rollout group.
#[derive(Debug, Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RolloutOrder(u32);

impl RolloutOrder {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn value(self) -> u32 {
        self.0
    }
}

impl From<u32> for RolloutOrder {
    fn from(value: u32) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for RolloutOrder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Fleet {
    pub version: u32,
    pub hosts: Vec<Host>,
    pub groups: BTreeMap<GroupName, Group>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub name: HostName,
    #[serde(default)]
    pub group: Option<GroupName>,
    pub order: RolloutOrder,
    pub target: Target,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub extensions: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "healthChecks", default)]
    pub health_checks: Vec<HealthCheck>,
    #[serde(rename = "expectedBuild")]
    pub desired_system_closure: SystemClosure,
    #[serde(rename = "configurationRevision", default)]
    pub configuration_revision: Option<ConfigurationRevision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub hosts: Vec<HostName>,
}

/// A named rollout group with its hosts in deployment order.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RolloutGroup {
    pub name: GroupName,
    pub hosts: Vec<HostName>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Target {
    pub host: Option<String>,
    pub port: u16,
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    pub name: String,
    #[serde(default)]
    pub command: Option<CommandCheck>,
    #[serde(default)]
    pub location: Option<CheckLocation>,
    pub timeout: u32,
    #[serde(default = "default_check_interval")]
    pub interval: u32,
    #[serde(default)]
    pub http: Option<HttpCheck>,
    #[serde(default)]
    pub tcp: Option<TcpCheck>,
    #[serde(default)]
    pub grpc: Option<GrpcCheck>,
}

fn default_check_interval() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCheck {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(rename = "expectedStatus", default)]
    pub expected_status: i32,
    #[serde(rename = "expectedStdout", default)]
    pub expected_stdout: Option<StringMatch>,
    #[serde(rename = "expectedStderr", default)]
    pub expected_stderr: Option<StringMatch>,
    #[serde(rename = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckLocation {
    /// Run the probe where cattix itself is running.
    Controller,
    /// Run the probe on the host whose deployment is being verified.
    Target,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpCheck {
    pub url: String,
    #[serde(default)]
    pub method: HttpMethod,
    #[serde(rename = "expectedStatus", default)]
    pub expected_status: Option<u16>,
    #[serde(rename = "expectedStatusRange", default)]
    pub expected_status_range: Option<StatusRange>,
    #[serde(rename = "expectedHeaders", default)]
    pub expected_headers: BTreeMap<String, StringMatch>,
    #[serde(rename = "expectedBody", default)]
    pub expected_body: Option<StringMatch>,
    #[serde(rename = "expectedJson", default)]
    pub expected_json: Option<JsonMatch>,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(rename = "tlsInsecure", default)]
    pub tls_insecure: bool,
    #[serde(rename = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRange {
    pub min: u16,
    pub max: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonMatch {
    pub path: String,
    pub matcher: StringMatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StringMatchKind {
    Exact,
    ExactInsensitive,
    Contains,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StringMatch {
    #[serde(rename = "type")]
    pub kind: StringMatchKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HttpMethod {
    Options,
    #[default]
    Get,
    Post,
    Put,
    Delete,
    Head,
    Trace,
    Connect,
    Patch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpCheck {
    pub addr: String,
    #[serde(rename = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcCheck {
    pub url: String,
    pub service: String,
    #[serde(rename = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostState {
    InSync,
    Behind,
    Drifted,
    Unreachable,
}

impl Host {
    pub fn state_for(&self, active_system_closure: Option<&str>) -> HostState {
        match active_system_closure {
            None => HostState::Unreachable,
            Some(build) if build == self.desired_system_closure.as_str() => HostState::InSync,
            Some(_) => HostState::Drifted,
        }
    }
}

impl Fleet {
    /// Decodes the versioned fleet document emitted by a desired-state source.
    ///
    /// The source may be Nix today and an API or saved document tomorrow; the
    /// fleet contract remains owned and validated by the core.
    pub fn from_json_slice(document: &[u8]) -> Result<Self> {
        let fleet: Self = serde_json::from_slice(document).context("parsing fleet JSON")?;
        fleet.validate()?;
        Ok(fleet)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported fleet document version {}", self.version);
        }

        let mut names = std::collections::BTreeSet::new();
        for host in &self.hosts {
            if host.name.as_str().is_empty() {
                bail!("fleet contains a host with an empty name");
            }
            if !names.insert(&host.name) {
                bail!("fleet contains duplicate host {}", host.name);
            }
            for check in &host.health_checks {
                check.validate().with_context(|| {
                    format!("invalid health check {} for host {}", check.name, host.name)
                })?;
            }
        }
        Ok(())
    }

    // Return a list of hosts ordered by group, then order, then name.
    // Optionally filtered by group.
    pub fn ordered_hosts(&self, group: Option<&GroupName>) -> Vec<&Host> {
        let mut hosts: Vec<_> = self
            .hosts
            .iter()
            .filter(|host| group.is_none_or(|selected| host.group.as_ref() == Some(selected)))
            .collect();

        hosts.sort_by(|left, right| {
            left.group
                .cmp(&right.group)
                .then(left.order.cmp(&right.order))
                .then(left.name.cmp(&right.name))
        });

        hosts
    }

    pub fn rollout_groups(&self) -> Vec<RolloutGroup> {
        let mut groups: BTreeMap<_, Vec<_>> = self
            .groups
            .keys()
            .map(|name| (name.clone(), Vec::new()))
            .collect();

        for host in self.ordered_hosts(None) {
            if let Some(group) = &host.group {
                groups
                    .entry(group.clone())
                    .or_default()
                    .push(host.name.clone());
            }
        }

        groups
            .into_iter()
            .map(|(name, hosts)| RolloutGroup { name, hosts })
            .collect()
    }

    pub fn selected_hosts<'a>(
        &'a self,
        scope: DeploymentScope<'_>,
    ) -> Result<impl Iterator<Item = &'a Host>> {
        let hosts = match scope {
            DeploymentScope::All => Ok(self.ordered_hosts(None)),
            DeploymentScope::Group(group) => Ok(self.ordered_hosts(Some(group))),
            DeploymentScope::Host(host_name) => self
                .hosts
                .iter()
                .find(|host| &host.name == host_name)
                .map(|host| vec![host])
                .ok_or_else(|| anyhow!("host not found: {host_name}")),
        }?;

        Ok(hosts.into_iter())
    }
}

impl HealthCheck {
    fn validate(&self) -> Result<()> {
        let configured_kinds = [
            self.command.is_some(),
            self.http.is_some(),
            self.tcp.is_some(),
            self.grpc.is_some(),
        ]
        .into_iter()
        .filter(|configured| *configured)
        .count();
        if configured_kinds != 1 {
            bail!("must configure exactly one of command, http, tcp, or grpc");
        }
        if self.timeout == 0 {
            bail!("timeout must be greater than zero");
        }
        if self.interval == 0 {
            bail!("interval must be greater than zero");
        }
        if let Some(http) = &self.http {
            match (&http.expected_status, &http.expected_status_range) {
                (Some(_), Some(_)) => {
                    bail!("HTTP probe configures both expectedStatus and expectedStatusRange")
                }
                (None, Some(range)) if range.min > range.max => {
                    bail!("HTTP probe has an expectedStatusRange whose minimum exceeds its maximum")
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl HostState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in-sync",
            Self::Behind => "behind",
            Self::Drifted => "drifted",
            Self::Unreachable => "unreachable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, group: Option<&str>, order: u32, desired_system_closure: &str) -> Host {
        Host {
            name: name.into(),
            group: group.map(GroupName::from),
            order: order.into(),
            target: Target {
                host: Some("127.0.0.1".into()),
                port: 22,
                user: "root".into(),
            },
            metadata: BTreeMap::new(),
            extensions: BTreeMap::new(),
            health_checks: Vec::new(),
            desired_system_closure: desired_system_closure.into(),
            configuration_revision: None,
        }
    }

    #[test]
    fn orders_hosts_by_group_order_and_name() {
        let fleet = Fleet {
            version: 1,
            hosts: vec![
                host("primary", Some("app"), 2, "/nix/store/app-2"),
                host("standby", Some("app"), 1, "/nix/store/app-1"),
                host("database", Some("db"), 1, "/nix/store/db-1"),
            ],
            groups: BTreeMap::new(),
        };

        let app = GroupName::from("app");
        let names: Vec<_> = fleet
            .ordered_hosts(Some(&app))
            .into_iter()
            .map(|host| host.name.as_str())
            .collect();
        assert_eq!(names, ["standby", "primary"]);
    }

    #[test]
    fn classifies_active_system_closure() {
        let host = host("app", Some("app"), 1, "/nix/store/app-1");
        assert!(matches!(host.state_for(None), HostState::Unreachable));
        assert!(matches!(
            host.state_for(Some("/nix/store/app-1")),
            HostState::InSync
        ));
        assert!(matches!(
            host.state_for(Some("/nix/store/app-manual")),
            HostState::Drifted
        ));
    }

    #[test]
    fn decodes_and_validates_the_fleet_document_in_core() {
        let fleet = Fleet::from_json_slice(br#"{"version":1,"hosts":[],"groups":{}}"#)
            .expect("a supported fleet document is accepted");
        assert_eq!(fleet.version, 1);

        let error = Fleet::from_json_slice(br#"{"version":2,"hosts":[],"groups":{}}"#)
            .expect_err("unsupported versions are rejected by the core");
        assert!(error
            .to_string()
            .contains("unsupported fleet document version"));
    }

    #[test]
    fn rejects_malformed_health_checks_while_loading_the_fleet() {
        let document = br#"{
            "version": 1,
            "hosts": [{
                "name": "app",
                "order": 0,
                "target": { "host": "127.0.0.1", "port": 22, "user": "root" },
                "expectedBuild": "/nix/store/app",
                "healthChecks": [{ "name": "invalid", "timeout": 1 }]
            }],
            "groups": {}
        }"#;

        let error = Fleet::from_json_slice(document)
            .expect_err("a health check without a probe kind must be rejected");
        assert!(format!("{error:#}")
            .contains("must configure exactly one of command, http, tcp, or grpc"));
    }

    #[test]
    fn system_closure_remains_a_json_string() {
        let closure = SystemClosure::new("/nix/store/example-system");
        assert_eq!(
            serde_json::to_string(&closure).expect("a closure serializes"),
            r#""/nix/store/example-system""#
        );
        assert_eq!(
            serde_json::from_str::<SystemClosure>(r#""/nix/store/example-system""#)
                .expect("a closure deserializes")
                .as_str(),
            "/nix/store/example-system"
        );
    }

    #[test]
    fn domain_value_types_preserve_the_fleet_json_scalars() {
        assert_eq!(
            serde_json::to_string(&HostName::from("app-primary")).expect("a host name serializes"),
            r#""app-primary""#
        );
        assert_eq!(
            serde_json::to_string(&GroupName::from("app")).expect("a group name serializes"),
            r#""app""#
        );
        assert_eq!(
            serde_json::to_string(&ConfigurationRevision::from("deadbeef"))
                .expect("a configuration revision serializes"),
            r#""deadbeef""#
        );
        assert_eq!(
            serde_json::to_string(&FlakeRef::from("github:example/fleet"))
                .expect("a flake reference serializes"),
            r#""github:example/fleet""#
        );
        assert_eq!(
            serde_json::to_string(&RolloutOrder::new(2)).expect("a rollout order serializes"),
            "2"
        );
    }
}
