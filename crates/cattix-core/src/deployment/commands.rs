use crate::{CommandCheck, SystemClosure};

pub(super) fn target_command(command: &CommandCheck) -> String {
    let executable = std::iter::once(&command.program)
        .chain(command.args.iter())
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    match command.timeout_ms {
        Some(timeout_ms) => format!("timeout --foreground {timeout_ms}ms {executable}"),
        None => executable,
    }
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
