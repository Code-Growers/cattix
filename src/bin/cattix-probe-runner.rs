use std::io::Read;

use anyhow::{Context, Result};
use cattix_core::{run_local_health_check, HealthCheck, TargetProbeResult};

fn main() -> Result<()> {
    let mut request = String::new();
    std::io::stdin()
        .read_to_string(&mut request)
        .context("reading target probe request")?;
    let response = match serde_json::from_str::<HealthCheck>(&request) {
        Ok(check) => match run_local_health_check(&check) {
            Ok(()) => TargetProbeResult {
                ok: true,
                error: None,
            },
            Err(error) => TargetProbeResult {
                ok: false,
                error: Some(format!("{error:#}")),
            },
        },
        Err(error) => TargetProbeResult {
            ok: false,
            error: Some(format!("invalid target probe request: {error}")),
        },
    };
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}
