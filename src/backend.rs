use crate::claude_cli;
use crate::config::Backend;
use crate::copilot_cli;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// Null device path, platform-specific. Used to override git config paths.
pub const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Git subcommands denied by the Copilot backend (`--deny-tool=shell(git <subcmd>)`).
/// The Claude backend uses a blanket `Bash(git:*)` pattern instead (see claude_cli.rs).
pub const DENIED_GIT_SUBCOMMANDS: &[&str] = &[
    "commit",
    "push",
    "pull",
    "fetch",
    "remote",
    "rebase",
    "reset",
    "clean",
    "merge",
    "checkout",
    "switch",
    "restore",
    "apply",
    "cherry-pick",
    "revert",
    "rm",
    "branch",
    "tag",
    "stash",
    "config",
    "add",
    "update-index",
    "mv",
    "worktree",
    "init",
    "submodule",
];

/// Build and execute an agent command, returning its output.
///
/// For the Claude backend, the prompt is delivered via stdin to avoid leaking
/// source code in `ps` output on multi-user systems and to sidestep OS `ARG_MAX`
/// limits for large diffs.
///
/// For the Copilot backend, the prompt is passed as a CLI argument (`-p`).
/// This is visible in `ps` output -- a known limitation documented in the README.
/// Default agent timeout: 10 minutes.
const AGENT_TIMEOUT_SECS: u64 = 600;

pub async fn run_agent(
    backend: &Backend,
    prompt: &str,
    model: &str,
    repo_root: &Path,
    state_dir: &Path,
) -> std::io::Result<std::process::Output> {
    let timeout_dur = std::time::Duration::from_secs(AGENT_TIMEOUT_SECS);
    match tokio::time::timeout(timeout_dur, run_agent_inner(backend, prompt, model, repo_root, state_dir)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "Agent timed out after {} seconds. The child process may be hung \
                 (network stall, API outage, or infinite tool-use loop).",
                AGENT_TIMEOUT_SECS
            ),
        )),
    }
}

async fn run_agent_inner(
    backend: &Backend,
    prompt: &str,
    model: &str,
    repo_root: &Path,
    state_dir: &Path,
) -> std::io::Result<std::process::Output> {
    match backend {
        Backend::Copilot => {
            // Copilot passes prompt as a CLI argument; warn about ARG_MAX risk.
            // macOS ARG_MAX is ~1 MiB total (including environment). Warn at
            // a lower threshold there to account for environment overhead.
            let warn_threshold: usize = if cfg!(target_os = "macos") { 250_000 } else { 900_000 };
            if prompt.len() > warn_threshold {
                eprintln!(
                    "Warning: prompt is very large ({} bytes). \
                     This may exceed OS argument-size limits and cause the agent to fail to start.",
                    prompt.len()
                );
            }
            // Use explicit spawn + wait_with_output so the child handle has
            // kill_on_drop(true). If the timeout fires and this future is
            // dropped, the child is killed automatically instead of leaking.
            use std::process::Stdio;
            let mut cmd = copilot_cli::command(prompt, model, repo_root, state_dir);
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            cmd.kill_on_drop(true);
            let child = cmd.spawn()?;
            child.wait_with_output().await
        }
        Backend::ClaudeCode => {
            use std::process::Stdio;
            use tokio::io::AsyncWriteExt;
            let mut cmd = claude_cli::command(model, repo_root, state_dir);
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            cmd.kill_on_drop(true);
            let mut child = cmd.spawn()?;
            // Spawn stdin write in a separate task so wait_with_output() can
            // drain stdout/stderr concurrently. Without this, large prompts
            // that exceed the OS pipe buffer (~64 KiB Linux, ~16 KiB macOS)
            // deadlock: the parent blocks on write_all waiting for the child
            // to read stdin, while the child blocks on stdout/stderr writes
            // waiting for the parent to drain them.
            let stdin = child.stdin.take();
            let prompt_owned = prompt.as_bytes().to_vec();
            let write_handle = tokio::spawn(async move {
                if let Some(mut stdin) = stdin {
                    let res = stdin.write_all(&prompt_owned).await;
                    drop(stdin); // close stdin to signal EOF
                    res
                } else {
                    Ok(())
                }
            });
            let output = child.wait_with_output().await?;
            // Check write result. BrokenPipe is expected if the child exits
            // before consuming all input (e.g. early validation failure).
            match write_handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Ok(Err(e)) => return Err(e),
                Err(join_err) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("stdin writer task panicked: {}", join_err),
                    ));
                }
            }
            Ok(output)
        }
    }
}

/// Strip ANSI escape sequences and control characters from a string.
///
/// Pass 1: strip ANSI escape sequences using the `strip-ansi-escapes` crate,
/// which uses a proper VT100 state-machine parser (via `vte`). This handles
/// CSI, OSC (both BEL and ST terminated), character-set selection, keypad
/// modes, and all other standard escape sequences.
///
/// Pass 2: strip any remaining control characters (bare BEL, NUL, etc.) that
/// are not part of escape sequences but have no role in markdown content.
/// Preserves newline (0x0A), tab (0x09), and carriage return (0x0D).
pub fn strip_ansi_codes(s: &str) -> String {
    let stripped = strip_ansi_escapes::strip(s);
    let stripped = String::from_utf8_lossy(&stripped);

    // Catch bare control bytes that aren't escape sequences.
    static CTRL: OnceLock<Regex> = OnceLock::new();
    let ctrl = CTRL.get_or_init(|| {
        Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap()
    });
    ctrl.replace_all(&stripped, "").to_string()
}

/// Check if an I/O error is E2BIG (argument list too long).
pub fn is_arg_too_long(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ArgumentListTooLong
}
