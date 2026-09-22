use super::{
    adapters::{CommandOutput, Transport},
    commands::{shell_quote, target_command},
    TargetProbeResult,
};
use crate::{
    CheckLocation, CommandCheck, GrpcCheck, HealthCheck, Host, HttpCheck, HttpMethod, StringMatch,
    StringMatchKind, TcpCheck,
};
use anyhow::{anyhow, bail, Context, Result};
use std::time::{Duration, Instant};
use zeronine_probes::{
    CommandMatcher, CommandProbe, CommandProbeConfiguration, GrpcProbe, GrpcProbeConfiguration,
    HttpMatcher, HttpProbe, HttpProbeConfiguration, IntMatcher, Method as ZeronineHttpMethod,
    Probe, StringMatcher as ZeronineStringMatcher, TcpProbe, TcpProbeConfiguration,
    TlsConfiguration,
};

pub(super) fn run_health_checks(
    transport: &dyn Transport,
    host: &Host,
    checks: &[HealthCheck],
    target_probe_runner: Option<&str>,
    on_log: &mut dyn FnMut(&str),
) -> Result<()> {
    for check in checks {
        let deadline = Instant::now() + Duration::from_secs(u64::from(check.timeout.max(1)));
        let mut attempt = 0;
        let mut last_error = anyhow!("health check did not run");

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("health check {} failed: {last_error:#}", check.name);
            }
            attempt += 1;
            on_log(&format!(
                "{}: attempt {attempt} on {}",
                check.name,
                check_location_name(check_location(check)),
            ));

            let attempt_check = with_attempt_timeout(check, remaining);
            match run_health_check(transport, host, &attempt_check, target_probe_runner) {
                Ok(()) => {
                    on_log(&format!("{}: passed on attempt {attempt}", check.name));
                    break;
                }
                Err(error) => {
                    on_log(&format!(
                        "{}: attempt {attempt} failed: {error:#}",
                        check.name
                    ));
                    last_error = error;
                }
            }

            if Instant::now() >= deadline {
                bail!("health check {} failed: {last_error:#}", check.name);
            }

            let interval = Duration::from_secs(u64::from(check.interval.max(1)));
            std::thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
    Ok(())
}

/// Runs a single check in the current network namespace. The target runner uses
/// this to share ZeroNine configuration and execution logic with the controller.
pub fn run_local_health_check(check: &HealthCheck) -> Result<()> {
    validate_check_kind(check)?;
    match (&check.command, &check.http, &check.tcp, &check.grpc) {
        (Some(command), None, None, None) => run_controller_command_check(command),
        (None, Some(http), None, None) => run_http_check(http),
        (None, None, Some(tcp), None) => run_tcp_check(tcp),
        (None, None, None, Some(grpc)) => run_grpc_check(grpc),
        _ => unreachable!("health-check kind count was validated"),
    }
}

pub(super) fn requires_target_probe_runner(checks: &[HealthCheck]) -> bool {
    checks.iter().any(|check| {
        matches!(check_location(check), CheckLocation::Target)
            && (check.http.is_some() || check.tcp.is_some() || check.grpc.is_some())
    })
}

fn with_attempt_timeout(check: &HealthCheck, remaining: Duration) -> HealthCheck {
    let mut bounded = check.clone();
    if let Some(command) = &mut bounded.command {
        command.timeout_ms = Some(bounded_timeout(command.timeout_ms, remaining));
    }
    if let Some(http) = &mut bounded.http {
        http.timeout_ms = Some(bounded_timeout(http.timeout_ms, remaining));
    }
    if let Some(tcp) = &mut bounded.tcp {
        tcp.timeout_ms = Some(bounded_timeout(tcp.timeout_ms, remaining));
    }
    if let Some(grpc) = &mut bounded.grpc {
        grpc.timeout_ms = Some(bounded_timeout(grpc.timeout_ms, remaining));
    }
    bounded
}

fn bounded_timeout(configured: Option<u64>, remaining: Duration) -> u64 {
    let remaining = u64::try_from(remaining.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    configured.map_or(remaining, |timeout| timeout.min(remaining))
}

fn run_health_check(
    transport: &dyn Transport,
    host: &Host,
    check: &HealthCheck,
    target_probe_runner: Option<&str>,
) -> Result<()> {
    validate_check_kind(check)?;
    let location = check_location(check);
    let result = match (&check.command, &check.http, &check.tcp, &check.grpc) {
        (Some(command), None, None, None) => match location {
            CheckLocation::Controller => run_controller_command_check(command),
            CheckLocation::Target => run_target_command_check(transport, host, command),
        },
        (None, Some(http), None, None) => run_network_check(
            location,
            transport,
            host,
            check,
            target_probe_runner,
            || run_http_check(http),
        ),
        (None, None, Some(tcp), None) => run_network_check(
            location,
            transport,
            host,
            check,
            target_probe_runner,
            || run_tcp_check(tcp),
        ),
        (None, None, None, Some(grpc)) => run_network_check(
            location,
            transport,
            host,
            check,
            target_probe_runner,
            || run_grpc_check(grpc),
        ),
        _ => unreachable!("health-check kind count was validated"),
    };
    result.with_context(|| format!("running health check {} on {}", check.name, host.name))
}

fn validate_check_kind(check: &HealthCheck) -> Result<()> {
    let configured_kinds = [
        check.command.is_some(),
        check.http.is_some(),
        check.tcp.is_some(),
        check.grpc.is_some(),
    ]
    .into_iter()
    .filter(|configured| *configured)
    .count();
    if configured_kinds != 1 {
        bail!(
            "health check {} must configure exactly one of command, http, tcp, or grpc",
            check.name
        );
    }
    Ok(())
}

fn run_network_check(
    location: CheckLocation,
    transport: &dyn Transport,
    host: &Host,
    check: &HealthCheck,
    target_probe_runner: Option<&str>,
    controller_check: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match location {
        CheckLocation::Controller => controller_check(),
        CheckLocation::Target => run_target_probe_check(
            transport,
            host,
            target_probe_runner.context("target probe runner was not copied")?,
            check,
        ),
    }
}

fn run_target_probe_check(
    transport: &dyn Transport,
    host: &Host,
    runner: &str,
    check: &HealthCheck,
) -> Result<()> {
    let request = serde_json::to_vec(check).context("encoding target probe request")?;
    let output = transport
        .run_with_input(host, &shell_quote(runner), &request)
        .context("starting target probe runner")?;
    let response: TargetProbeResult =
        serde_json::from_str(&output.stdout).context("decoding target probe runner response")?;
    if response.ok {
        return Ok(());
    }

    let error = response.error.unwrap_or_else(|| {
        if output.stderr.is_empty() {
            format!("target probe runner exited with status {}", output.status)
        } else {
            output.stderr
        }
    });
    bail!(error)
}

fn run_controller_command_check(command: &CommandCheck) -> Result<()> {
    let probe = CommandProbe::try_new(&CommandProbeConfiguration {
        command: command.program.clone(),
        args: command.args.clone(),
        timeout_ms: command.timeout_ms.map(Duration::from_millis),
        expected_status: command.expected_status,
        matchers: command_matchers(command),
    })
    .map_err(|error| anyhow!(error))?;
    run_controller_probe(probe)
}

fn run_http_check(check: &HttpCheck) -> Result<()> {
    let probe = HttpProbe::try_new(&HttpProbeConfiguration {
        url: check.url.clone(),
        method: zeronine_http_method(check.method),
        proxy: check.proxy.clone(),
        tls: check
            .tls_insecure
            .then_some(TlsConfiguration { skip_verify: true }),
        matchers: Some(http_matchers(check)?),
        timeout_ms: check.timeout_ms.map(Duration::from_millis),
    })
    .map_err(|error| anyhow!(error))?;
    run_controller_probe(probe)
}

fn run_tcp_check(check: &TcpCheck) -> Result<()> {
    let probe = TcpProbe::try_new(&TcpProbeConfiguration {
        addr: check.addr.clone(),
        timeout_ms: check.timeout_ms.map(Duration::from_millis),
    })
    .map_err(|error| anyhow!(error))?;
    run_controller_probe(probe)
}

fn run_grpc_check(check: &GrpcCheck) -> Result<()> {
    let probe = GrpcProbe::try_new(&GrpcProbeConfiguration {
        url: check.url.clone(),
        service: check.service.clone(),
        timeout_ms: check.timeout_ms.map(Duration::from_millis),
    })
    .map_err(|error| anyhow!(error))?;
    run_controller_probe(probe)
}

fn run_controller_probe(probe: impl Probe) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating runtime for controller probe")?;
    runtime
        .block_on(probe.probe())
        .map_err(|error| anyhow!(error))?;
    Ok(())
}

fn run_target_command_check(
    transport: &dyn Transport,
    host: &Host,
    command: &CommandCheck,
) -> Result<()> {
    let output = transport.run(host, &target_command(command))?;
    if output.status == command.expected_status {
        match_target_command_output(command, &output)?;
        return Ok(());
    }
    bail!(command_output_error(&output, "target command"))
}

fn command_matchers(command: &CommandCheck) -> Option<Vec<CommandMatcher>> {
    let mut matchers = Vec::new();
    if let Some(matcher) = &command.expected_stdout {
        matchers.push(CommandMatcher::Stdout(zeronine_string_matcher(matcher)));
    }
    if let Some(matcher) = &command.expected_stderr {
        matchers.push(CommandMatcher::Stderr(zeronine_string_matcher(matcher)));
    }
    (!matchers.is_empty()).then_some(matchers)
}

fn match_target_command_output(command: &CommandCheck, output: &CommandOutput) -> Result<()> {
    match_command_stream("stdout", &output.stdout, command.expected_stdout.as_ref())?;
    match_command_stream("stderr", &output.stderr, command.expected_stderr.as_ref())
}

fn match_command_stream(stream: &str, output: &str, matcher: Option<&StringMatch>) -> Result<()> {
    let Some(matcher) = matcher else {
        return Ok(());
    };
    if zeronine_string_matcher(matcher).do_match(output) {
        return Ok(());
    }
    bail!(
        "target command {stream} did not match {:?} {:?}: {:?}",
        matcher.kind,
        matcher.value,
        output
    )
}

fn http_matchers(check: &HttpCheck) -> Result<Vec<HttpMatcher>> {
    let mut matchers = vec![HttpMatcher::Status(http_status_matcher(check)?)];
    matchers.extend(check.expected_headers.iter().map(|(name, matcher)| {
        HttpMatcher::Header(name.clone(), zeronine_string_matcher(matcher))
    }));
    if let Some(matcher) = &check.expected_body {
        matchers.push(HttpMatcher::TextBody(zeronine_string_matcher(matcher)));
    }
    if let Some(json) = &check.expected_json {
        matchers.push(HttpMatcher::JsonBody(
            json.path.clone(),
            zeronine_string_matcher(&json.matcher),
        ));
    }
    Ok(matchers)
}

fn http_status_matcher(check: &HttpCheck) -> Result<IntMatcher<u64>> {
    match (&check.expected_status, &check.expected_status_range) {
        (Some(_), Some(_)) => bail!(
            "HTTP probe {} configures both expectedStatus and expectedStatusRange",
            check.url
        ),
        (Some(status), None) => Ok(IntMatcher::Exact(u64::from(*status))),
        (None, Some(range)) if range.min > range.max => bail!(
            "HTTP probe {} has an invalid expectedStatusRange: {} is greater than {}",
            check.url,
            range.min,
            range.max
        ),
        (None, Some(range)) => Ok(IntMatcher::Range(
            u64::from(range.min),
            u64::from(range.max),
        )),
        (None, None) => Ok(IntMatcher::Exact(200)),
    }
}

fn zeronine_http_method(method: HttpMethod) -> ZeronineHttpMethod {
    match method {
        HttpMethod::Options => ZeronineHttpMethod::Options,
        HttpMethod::Get => ZeronineHttpMethod::Get,
        HttpMethod::Post => ZeronineHttpMethod::Post,
        HttpMethod::Put => ZeronineHttpMethod::Put,
        HttpMethod::Delete => ZeronineHttpMethod::Delete,
        HttpMethod::Head => ZeronineHttpMethod::Head,
        HttpMethod::Trace => ZeronineHttpMethod::Trace,
        HttpMethod::Connect => ZeronineHttpMethod::Connect,
        HttpMethod::Patch => ZeronineHttpMethod::Patch,
    }
}

fn zeronine_string_matcher(matcher: &StringMatch) -> ZeronineStringMatcher {
    match matcher.kind {
        StringMatchKind::Exact => ZeronineStringMatcher::Exact(matcher.value.clone()),
        StringMatchKind::ExactInsensitive => {
            ZeronineStringMatcher::ExactInsensitive(matcher.value.clone())
        }
        StringMatchKind::Contains => ZeronineStringMatcher::Contains(matcher.value.clone()),
    }
}

fn check_location(check: &HealthCheck) -> CheckLocation {
    check.location.unwrap_or_else(|| {
        if check.command.is_some() {
            CheckLocation::Target
        } else {
            CheckLocation::Controller
        }
    })
}

fn check_location_name(location: CheckLocation) -> &'static str {
    match location {
        CheckLocation::Controller => "controller",
        CheckLocation::Target => "target",
    }
}

fn command_output_error(output: &CommandOutput, label: &str) -> String {
    let mut detail = format!("{label} exited with status {}", output.status);
    if !output.stderr.is_empty() {
        detail.push_str(&format!(": {}", output.stderr));
    } else if !output.stdout.is_empty() {
        detail.push_str(&format!(": {}", output.stdout));
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_check(timeout_ms: Option<u64>) -> HealthCheck {
        HealthCheck {
            name: "command".into(),
            command: Some(CommandCheck {
                program: "true".into(),
                args: Vec::new(),
                expected_status: 0,
                expected_stdout: None,
                expected_stderr: None,
                timeout_ms,
            }),
            location: None,
            timeout: 30,
            interval: 1,
            http: None,
            tcp: None,
            grpc: None,
        }
    }

    #[test]
    fn attempt_timeout_defaults_to_and_is_capped_by_the_remaining_deadline() {
        let unbounded = with_attempt_timeout(&command_check(None), Duration::from_millis(250));
        assert_eq!(unbounded.command.unwrap().timeout_ms, Some(250));

        let capped = with_attempt_timeout(&command_check(Some(1_000)), Duration::from_millis(250));
        assert_eq!(capped.command.unwrap().timeout_ms, Some(250));

        let preserved = with_attempt_timeout(&command_check(Some(100)), Duration::from_millis(250));
        assert_eq!(preserved.command.unwrap().timeout_ms, Some(100));
    }
}
