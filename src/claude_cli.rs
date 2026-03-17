use std::path::Path;
use tokio::process::Command;

/// Build a Claude CLI command.
///
/// The `--disallowed-tools` flag removes named tools from the agent's tool set.
/// Per `claude --help`, it accepts a "Comma or space-separated list of tool names
/// to deny (e.g. `Bash(git:*) Edit`)". We use `Bash(git:*)` to block **all** git
/// operations. Per-subcommand patterns like `Bash(git commit:*)` are not documented
/// as supported and may silently fail, so the blanket `Bash(git:*)` is used for safety.
///
/// The prompt is NOT included as a CLI argument. Callers must deliver the prompt
/// via stdin (see `backend::run_agent`). This avoids leaking source code in `ps`
/// output on multi-user systems and sidesteps OS `ARG_MAX` limits for large diffs.
///
/// **Known limitations** (also documented in README):
/// - The pattern only matches git invocations through the Bash tool. An LLM can
///   bypass via indirect invocations (`env git commit`, `bash -c "git commit"`,
///   `/usr/bin/git commit`, shell aliases, etc.).
/// - Non-git destructive commands (`rm`, `curl | sh`) are NOT blocked.
/// - The `--dangerously-skip-permissions` flag is required for non-interactive
///   operation but grants broad shell access. Treat agent output as untrusted.
pub fn command(model: &str, repo_root: &Path, state_dir: &Path) -> Command {
    let mut command = Command::new("claude");
    // Defense-in-depth: override git config paths to prevent the agent from
    // reading user aliases or writing persistent config via indirect invocation.
    command.env("GIT_CONFIG_GLOBAL", crate::backend::NULL_DEVICE);
    command.env("GIT_CONFIG_SYSTEM", crate::backend::NULL_DEVICE);
    command.current_dir(repo_root);
    command
        .arg("--print")
        .arg("--model")
        .arg(model)
        .arg("--add-dir")
        .arg(repo_root)
        .arg("--add-dir")
        .arg(state_dir)
        .arg("--disallowed-tools=Bash(git:*)")
        .arg("--dangerously-skip-permissions");
    command
}

/// Print a warning about the --dangerously-skip-permissions flag.
pub fn print_permissions_warning() {
    eprintln!("Warning: Claude Code backend runs with --dangerously-skip-permissions.");
    eprintln!("  The agent can execute shell commands without interactive confirmation.");
    eprintln!("  A deny list blocks destructive git operations, but other commands");
    eprintln!("  (rm, curl, etc.) are not restricted. Review agent output carefully.");
}

/// Flags that `command()` unconditionally passes to the Claude CLI.
/// `verify_disallowed_tools_support()` and the sync init-time check both use
/// this list so they stay in sync with what `command()` actually invokes.
pub const REQUIRED_CLI_FLAGS: &[&str] = &[
    "--print",
    "--disallowed-tools",
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
                 This flag is required for safe operation. \
                 Please upgrade your Claude CLI or use the Copilot backend.",
                flag
            ));
        }
    }
    Ok(())
}

/// Verify that the installed Claude CLI supports the flags passed by `command()`.
/// If any required flag is not recognized, the command would fail or silently
/// lose safety guarantees at runtime.
///
/// Checks `claude --help` output for all flags in `REQUIRED_CLI_FLAGS`.
/// This is a fast, offline check that does not make any API calls.
pub async fn verify_disallowed_tools_support() -> Result<(), String> {
    let help_output = Command::new("claude")
        .arg("--help")
        .output()
        .await
        .map_err(|e| format!("Failed to run 'claude --help': {}", e))?;
    let help_stdout = String::from_utf8_lossy(&help_output.stdout);
    let help_stderr = String::from_utf8_lossy(&help_output.stderr);
    check_required_flags(&help_stdout, &help_stderr)
}
