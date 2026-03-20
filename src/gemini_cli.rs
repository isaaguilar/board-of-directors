use std::path::Path;
use tokio::process::Command;

/// Build a Gemini CLI command.
///
/// The prompt is delivered via the `--prompt` argument. Large prompts may still
/// hit OS argument-size limits and will be visible in `ps` output.
pub async fn command(
    model: &str,
    working_dir: &Path,
    allow_repo_access: bool,
    use_sandbox: bool,
    repo_root: &Path,
    state_dir: &Path,
) -> std::io::Result<Command> {
    let mut command = Command::new("gemini");
    command.env("GIT_CONFIG_GLOBAL", crate::backend::NULL_DEVICE);
    command.env("GIT_CONFIG_SYSTEM", crate::backend::NULL_DEVICE);
    crate::backend::apply_node_heap_limit(&mut command);
    // Sanitize environment when repository access is not allowed to reduce
    // risk of deny-list bypass via child processes (unset git envs, use curated PATH).
    crate::backend::sanitize_command_env(&mut command, allow_repo_access, "gemini").await?;
    command.current_dir(working_dir);
    command
        .arg("--model")
        .arg(model)
        .arg("--approval-mode")
        .arg("yolo")
        .arg("--include-directories")
        .arg(state_dir)
        .arg("--output-format")
        .arg("text");
    if use_sandbox {
        command.arg("--sandbox");
    }
    if allow_repo_access {
        command.arg("--include-directories").arg(repo_root);
    }
    Ok(command)
}

pub const REQUIRED_CLI_FLAGS: &[&str] = &[
    "--model",
    "--prompt",
    "--approval-mode",
    "--include-directories",
    "--output-format",
];

pub fn check_required_flags(help_stdout: &str, help_stderr: &str) -> Result<(), String> {
    use regex::Regex;
    for flag in REQUIRED_CLI_FLAGS {
        let pattern = format!(r"(?:^|\s|,){}(?:\s|,|=|$)", regex::escape(flag));
        let re = Regex::new(&pattern).unwrap();
        if !re.is_match(help_stdout) && !re.is_match(help_stderr) {
            return Err(format!(
                "Your Gemini CLI does not support {}. This flag is required for non-interactive operation. Please upgrade your Gemini CLI or use another backend.",
                flag
            ));
        }
    }
    Ok(())
}

pub async fn verify_required_flags() -> Result<(), String> {
    let help_output = Command::new("gemini")
        .arg("--help")
        .output()
        .await
        .map_err(|e| format!("Failed to run 'gemini --help': {}", e))?;
    let help_stdout = String::from_utf8_lossy(&help_output.stdout);
    let help_stderr = String::from_utf8_lossy(&help_output.stderr);
    check_required_flags(&help_stdout, &help_stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_missing_required_flags() {
        let error = check_required_flags("--model", "").unwrap_err();
        assert!(error.contains("--prompt"));
    }

    #[test]
    fn accepts_help_output_with_required_flags() {
        let help = "--model --prompt --approval-mode --include-directories --output-format";
        assert!(check_required_flags(help, "").is_ok());
    }
}
