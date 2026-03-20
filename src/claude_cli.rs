use std::path::Path;
use tokio::process::Command;

/// Build a Claude CLI command.
///
/// The prompt is delivered via stdin instead of a CLI argument so large diffs do
/// not hit OS argument limits.
pub async fn command(
    model: &str,
    working_dir: &Path,
    allow_repo_access: bool,
    repo_root: &Path,
    state_dir: &Path,
) -> std::io::Result<Command> {
    let mut command = Command::new("claude");
    // Defense-in-depth: override git config paths to prevent the agent from
    // reading user aliases or writing persistent config via indirect invocation.
    command.env("GIT_CONFIG_GLOBAL", crate::backend::NULL_DEVICE);
    command.env("GIT_CONFIG_SYSTEM", crate::backend::NULL_DEVICE);
    // Apply Node heap limit to avoid OOMs on large inputs (same as other backends).
    crate::backend::apply_node_heap_limit(&mut command);
    // Sanitize environment for runs that should not access the repository.
    crate::backend::sanitize_command_env(&mut command, allow_repo_access, "claude").await?;
    command.current_dir(working_dir);
    command
        .arg("--print")
        .arg("--model")
        .arg(model)
        .arg("--add-dir")
        .arg(state_dir)
        .arg("--dangerously-skip-permissions");
    if allow_repo_access {
        command.arg("--add-dir").arg(repo_root);
    }
    Ok(command)
}

/// Flags that `command()` unconditionally passes to the Claude CLI.
/// `verify_required_flags()` and the sync init-time check both use
/// this list so they stay in sync with what `command()` actually invokes.
pub const REQUIRED_CLI_FLAGS: &[&str] = &[
    "--print",
    "--add-dir",
    "--dangerously-skip-permissions",
];

/// Verify that the given `claude --help` output contains all required CLI flags.
/// Shared between the async startup check and the sync init-time check to avoid
/// duplicating the verification logic.
pub fn check_required_flags(help_stdout: &str, help_stderr: &str) -> Result<(), String> {
    use regex::Regex;
    for flag in REQUIRED_CLI_FLAGS {
        // Use word-boundary-aware matching to avoid false positives like
        // `--print` matching inside `--no-print` or `--fingerprint`.
        let pattern = format!(r"(?:^|\s|,){}(?:\s|,|=|$)", regex::escape(flag));
        let re = Regex::new(&pattern).unwrap();
        if !re.is_match(help_stdout) && !re.is_match(help_stderr) {
            return Err(format!(
                "Your Claude CLI does not support {}. \
                 This flag is required for non-interactive operation. \
                 Please upgrade your Claude CLI or use the Copilot backend.",
                flag
            ));
        }
    }
    Ok(())
}

/// Verify that the installed Claude CLI supports the flags passed by `command()`.
/// Checks `claude --help` output for all flags in `REQUIRED_CLI_FLAGS`.
pub async fn verify_required_flags() -> Result<(), String> {
    let help_output = Command::new("claude")
        .arg("--help")
        .output()
        .await
        .map_err(|e| format!("Failed to run 'claude --help': {}", e))?;
    let help_stdout = String::from_utf8_lossy(&help_output.stdout);
    let help_stderr = String::from_utf8_lossy(&help_output.stderr);
    check_required_flags(&help_stdout, &help_stderr)
}
