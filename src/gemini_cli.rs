use std::path::Path;
use tokio::process::Command;

/// Build a Gemini CLI command.
///
/// The prompt is delivered via stdin and `--prompt ""` forces Gemini into
/// headless mode, so repository content is not exposed in `ps` output and
/// large prompts do not rely on OS argument-size limits.
///
/// Gemini CLI does not expose the same git-specific deny-list flags that the
/// Copilot and Claude CLIs do, so this integration leans on Gemini's sandbox
/// mode plus prompt-level restrictions. Treat agent output as untrusted.
pub fn command(model: &str, repo_root: &Path, state_dir: &Path) -> Command {
    let mut command = Command::new("gemini");
    command.env("GIT_CONFIG_GLOBAL", crate::backend::NULL_DEVICE);
    command.env("GIT_CONFIG_SYSTEM", crate::backend::NULL_DEVICE);
    command.current_dir(repo_root);
    command
        .arg("--model")
        .arg(model)
        .arg("--prompt")
        .arg("")
        .arg("--approval-mode")
        .arg("yolo")
        .arg("--sandbox")
        .arg("--include-directories")
        .arg(repo_root)
        .arg("--include-directories")
        .arg(state_dir)
        .arg("--output-format")
        .arg("text");
    command
}

pub fn print_permissions_warning() {
    eprintln!("Warning: Gemini CLI backend runs with --approval-mode yolo.");
    eprintln!("  Tool actions are auto-approved for non-interactive execution.");
    eprintln!("  bod also enables Gemini sandboxing, but Gemini CLI does not expose");
    eprintln!("  the same git-specific deny-list controls as Copilot or Claude.");
    eprintln!("  Review agent output carefully, especially for fix runs.");
}

pub const REQUIRED_CLI_FLAGS: &[&str] = &[
    "--model",
    "--prompt",
    "--approval-mode",
    "--sandbox",
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
                "Your Gemini CLI does not support {}. This flag is required for safe non-interactive operation. Please upgrade your Gemini CLI or use another backend.",
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
        let error = check_required_flags("--model --sandbox", "").unwrap_err();
        assert!(error.contains("--prompt"));
    }

    #[test]
    fn accepts_help_output_with_required_flags() {
        let help =
            "--model --prompt --approval-mode --sandbox --include-directories --output-format";
        assert!(check_required_flags(help, "").is_ok());
    }
}
