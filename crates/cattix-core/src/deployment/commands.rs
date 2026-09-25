use crate::{CommandCheck, SystemClosure};

pub(super) fn target_command(command: &CommandCheck) -> String {
    let executable = std::iter::once(&command.program)
        .chain(command.args.iter())
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    match command.timeout_ms {
        Some(timeout_ms) => format!(
            "timeout --foreground {} {executable}",
            timeout_duration(timeout_ms)
        ),
        None => executable,
    }
}

fn timeout_duration(timeout_ms: u64) -> String {
    format!("{}.{:03}s", timeout_ms / 1_000, timeout_ms % 1_000)
}

pub(super) fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\"'\"'"))
}

pub(super) fn activation_command(
    user: &str,
    build: &SystemClosure,
    previous: &SystemClosure,
) -> String {
    let build = shell_quote(build.as_str());
    let previous = shell_quote(previous.as_str());
    let command = format!(
        "set -eu; previous={previous}; printf '%s\\n' \"$previous\" >/run/cattix-previous-system; build={build}; nix-env --profile /nix/var/nix/profiles/system --set \"$build\"; \"$build/bin/switch-to-configuration\" switch"
    );
    background_command(user, &command, "/run/cattix-activation.log")
}

pub(super) fn rollback_command(user: &str) -> String {
    let command = "set -eu; previous=$(cat /run/cattix-previous-system); nix-env --profile /nix/var/nix/profiles/system --set \"$previous\"; if [ -x \"$previous/bin/switch-to-configuration\" ]; then \"$previous/bin/switch-to-configuration\" switch; else ln -sfn \"$previous\" /run/current-system; fi";
    background_command(user, command, "/run/cattix-rollback.log")
}

pub(super) fn acquire_lock_command(user: &str, token: &str) -> String {
    let token = shell_quote(token);
    let command = format!(
        "set -eu; lock=/run/cattix-deployment.lock; if mkdir \"$lock\" 2>/dev/null; then printf '%s\\n' {token} >\"$lock/owner\"; else printf 'cattix deployment lock is held by %s\\n' \"$(cat \"$lock/owner\" 2>/dev/null || printf unknown)\" >&2; exit 75; fi"
    );
    elevated_command(user, &command)
}

pub(super) fn release_lock_command(user: &str, token: &str) -> String {
    let token = shell_quote(token);
    let command = format!(
        "set -eu; lock=/run/cattix-deployment.lock; if [ ! -d \"$lock\" ]; then exit 0; fi; if [ \"$(cat \"$lock/owner\" 2>/dev/null || true)\" != {token} ]; then printf '%s\\n' 'cattix deployment lock ownership changed; leaving it in place' >&2; exit 1; fi; rm -f \"$lock/owner\"; rmdir \"$lock\""
    );
    elevated_command(user, &command)
}

fn background_command(user: &str, command: &str, log_path: &str) -> String {
    let command = elevated_command(user, command);
    format!(
        "nohup sh -c {} >{log_path} 2>&1 </dev/null &",
        shell_quote(&command)
    )
}

fn elevated_command(user: &str, command: &str) -> String {
    if user == "root" {
        command.to_owned()
    } else {
        format!("exec sudo -n sh -c {}", shell_quote(command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_commands_preserve_the_owner_token_and_do_not_remove_another_owner_lock() {
        let token = "pid=123; started=now";
        let acquire = acquire_lock_command("deploy", token);
        let release = release_lock_command("deploy", token);
        assert!(acquire.contains("mkdir \"$lock\""));
        assert!(acquire.contains("cattix deployment lock is held by"));
        assert!(release.contains("ownership changed; leaving it in place"));
        assert!(release.contains("sudo -n"));
        assert!(release.contains("'pid=123; started=now'"));
    }

    #[test]
    fn target_command_uses_a_gnu_timeout_duration() {
        let command = CommandCheck {
            program: "true".into(),
            args: vec![],
            expected_status: 0,
            expected_stdout: None,
            expected_stderr: None,
            timeout_ms: Some(9_999),
        };
        assert_eq!(
            target_command(&command),
            "timeout --foreground 9.999s 'true'"
        );
    }
}
