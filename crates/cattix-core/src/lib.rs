//! Fleet domain model and application service.

mod deployment;
mod model;

pub use deployment::{
    run_local_health_check, DeployEvent, DeployStep, DeploymentPlan, DeploymentReporter,
    DeploymentScope, FleetService, HostDiff, HostStatus, SystemClosureDiff, TargetProbeResult,
};
pub use model::{
    CheckLocation, CommandCheck, ConfigurationRevision, FlakeRef, Fleet, Group, GroupName,
    GrpcCheck, HealthCheck, Host, HostName, HostState, HttpCheck, HttpMethod, JsonMatch,
    RolloutGroup, RolloutOrder, StatusRange, StringMatch, StringMatchKind, SystemClosure, Target,
    TcpCheck,
};
