use crate::claude_cli;
use crate::config::Backend;
use crate::copilot_cli;
use crate::gemini_cli;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;

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

/// Default agent timeout: 10 minutes.
const AGENT_TIMEOUT_SECS: u64 = 600;
const RATE_LIMIT_MAX_RETRIES: usize = 3;
const RATE_LIMIT_FALLBACK_DELAYS_SECS: [u64; RATE_LIMIT_MAX_RETRIES] = [60, 120, 180];

pub enum AgentRunResult {
    Completed(std::process::Output),
    Cancelled,
}

struct RunningAgent {
    child: tokio::process::Child,
    stdout_handle: JoinHandle<std::io::Result<Vec<u8>>>,
    stderr_handle: JoinHandle<std::io::Result<Vec<u8>>>,
    stdin_handle: Option<JoinHandle<std::io::Result<()>>>,
}

pub async fn run_agent(
    backend: &Backend,
    prompt: &str,
    model: &str,
    repo_root: &Path,
    state_dir: &Path,
) -> std::io::Result<std::process::Output> {
    match run_agent_inner_with_cancel(backend, prompt, model, repo_root, state_dir, None).await? {
        AgentRunResult::Completed(output) => Ok(output),
        AgentRunResult::Cancelled => Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Agent was cancelled.",
        )),
    }
}

pub async fn run_agent_cancellable(
    backend: &Backend,
    prompt: &str,
    model: &str,
    repo_root: &Path,
    state_dir: &Path,
    cancel_rx: &mut watch::Receiver<bool>,
) -> std::io::Result<AgentRunResult> {
    run_agent_inner_with_cancel(
        backend,
        prompt,
        model,
        repo_root,
        state_dir,
        Some(cancel_rx),
    )
    .await
}

async fn run_agent_inner_with_cancel(
    backend: &Backend,
    prompt: &str,
    model: &str,
    repo_root: &Path,
    state_dir: &Path,
    mut cancel_rx: Option<&mut watch::Receiver<bool>>,
) -> std::io::Result<AgentRunResult> {
    let mut retry_count = 0usize;
    loop {
        let output = match run_agent_once_with_cancel(
            backend,
            prompt,
            model,
            repo_root,
            state_dir,
            cancel_rx.as_deref_mut(),
        )
        .await?
        {
            AgentRunResult::Completed(output) => output,
            AgentRunResult::Cancelled => return Ok(AgentRunResult::Cancelled),
        };

        if !should_retry_rate_limit(&output) {
            return Ok(AgentRunResult::Completed(output));
        }

        if retry_count >= RATE_LIMIT_MAX_RETRIES {
            eprintln!(
                "Rate limit persisted after {} retries for backend '{}' and model '{}'. Giving up.",
                RATE_LIMIT_MAX_RETRIES, backend, model
            );
            return Ok(AgentRunResult::Completed(output));
        }

        let wait = retry_delay_from_output(&output)
            .unwrap_or_else(|| Duration::from_secs(RATE_LIMIT_FALLBACK_DELAYS_SECS[retry_count]));
        retry_count += 1;
        eprintln!(
            "Rate limit detected from backend '{}' with model '{}'. Waiting {}s before retry {}/{}.",
            backend,
            model,
            wait.as_secs(),
            retry_count,
            RATE_LIMIT_MAX_RETRIES
        );

        if !sleep_with_cancel(wait, cancel_rx.as_deref_mut()).await? {
            return Ok(AgentRunResult::Cancelled);
        }
    }
}

async fn run_agent_once_with_cancel(
    backend: &Backend,
    prompt: &str,
    model: &str,
    repo_root: &Path,
    state_dir: &Path,
    mut cancel_rx: Option<&mut watch::Receiver<bool>>,
) -> std::io::Result<AgentRunResult> {
    let running = match backend {
        Backend::Copilot => {
            let warn_threshold: usize = if cfg!(target_os = "macos") {
                250_000
            } else {
                900_000
            };
            if prompt.len() > warn_threshold {
                eprintln!(
                    "Warning: prompt is very large ({} bytes). This may exceed OS argument-size limits and cause the agent to fail to start.",
                    prompt.len()
                );
            }
            let cmd = copilot_cli::command(prompt, model, repo_root, state_dir);
            spawn_command(cmd, None)?
        }
        Backend::ClaudeCode => {
            let cmd = claude_cli::command(model, repo_root, state_dir);
            spawn_command(cmd, Some(prompt.as_bytes().to_vec()))?
        }
        Backend::GeminiCli => {
            let cmd = gemini_cli::command(model, repo_root, state_dir);
            spawn_command(cmd, Some(prompt.as_bytes().to_vec()))?
        }
    };

    run_running_agent(running, cancel_rx.as_deref_mut()).await
}

async fn sleep_with_cancel(
    duration: Duration,
    mut cancel_rx: Option<&mut watch::Receiver<bool>>,
) -> std::io::Result<bool> {
    let sleep = tokio::time::sleep(duration);
    tokio::pin!(sleep);

    if let Some(cancel_rx_ref) = cancel_rx.as_deref_mut() {
        loop {
            tokio::select! {
                _ = &mut sleep => return Ok(true),
                changed = cancel_rx_ref.changed() => {
                    match changed {
                        Ok(()) if *cancel_rx_ref.borrow_and_update() => return Ok(false),
                        Err(_) => return Ok(true),
                        _ => {}
                    }
                }
            }
        }
    }

    sleep.await;
    Ok(true)
}

fn should_retry_rate_limit(output: &std::process::Output) -> bool {
    if output.status.success() {
        return false;
    }
    let combined = rate_limit_text(output);
    is_rate_limited_text(&combined)
}

fn rate_limit_text(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{}\n{}", stdout, stderr)
}

fn is_rate_limited_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("rate_limited")
        || lower.contains("resource exhausted")
        || lower.contains("resource_exhausted")
        || lower.contains("quota exceeded")
}

fn retry_delay_from_output(output: &std::process::Output) -> Option<Duration> {
    extract_retry_delay(&rate_limit_text(output))
}

fn extract_retry_delay(text: &str) -> Option<Duration> {
    static PHRASE_RE: OnceLock<Regex> = OnceLock::new();
    static GENERIC_RE: OnceLock<Regex> = OnceLock::new();
    let phrase_re = PHRASE_RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:retry(?: after| in)?|try again in|wait(?: for)?|available in|reset in)[^0-9]{0,20}(\d+(?:\.\d+)?)\s*(seconds?|secs?|s|minutes?|mins?|m)",
        )
        .unwrap()
    });
    if let Some(caps) = phrase_re.captures(text) {
        return duration_from_capture(&caps[1], &caps[2]);
    }

    if !is_rate_limited_text(text) {
        return None;
    }

    let generic_re = GENERIC_RE.get_or_init(|| {
        Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(seconds?|secs?|s|minutes?|mins?|m)").unwrap()
    });
    if let Some(caps) = generic_re.captures(text) {
        return duration_from_capture(&caps[1], &caps[2]);
    }
    None
}

fn duration_from_capture(value: &str, unit: &str) -> Option<Duration> {
    let value = value.parse::<f64>().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let seconds = match unit.to_ascii_lowercase().as_str() {
        "minute" | "minutes" | "min" | "mins" | "m" => value * 60.0,
        _ => value,
    };
    let seconds = seconds.ceil().max(1.0) as u64;
    Some(Duration::from_secs(seconds))
}

fn spawn_command(
    mut cmd: tokio::process::Command,
    stdin_payload: Option<Vec<u8>>,
) -> std::io::Result<RunningAgent> {
    use std::process::Stdio;
    cmd.stdin(if stdin_payload.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "Agent child stdout was not piped as expected.",
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "Agent child stderr was not piped as expected.",
        )
    })?;
    let stdin_handle = stdin_payload.map(|payload| {
        let stdin = child.stdin.take();
        tokio::spawn(async move {
            if let Some(mut stdin) = stdin {
                let res = stdin.write_all(&payload).await;
                drop(stdin);
                res
            } else {
                Ok(())
            }
        })
    });

    Ok(RunningAgent {
        child,
        stdout_handle: spawn_reader_task(stdout),
        stderr_handle: spawn_reader_task(stderr),
        stdin_handle,
    })
}

async fn run_running_agent(
    mut running: RunningAgent,
    mut cancel_rx: Option<&mut watch::Receiver<bool>>,
) -> std::io::Result<AgentRunResult> {
    let timeout = tokio::time::sleep(Duration::from_secs(AGENT_TIMEOUT_SECS));
    tokio::pin!(timeout);

    loop {
        if let Some(cancel_rx_ref) = cancel_rx.as_deref_mut() {
            tokio::select! {
                status = running.child.wait() => {
                    let output = finish_agent_output(status?, running).await?;
                    return Ok(AgentRunResult::Completed(output));
                }
                _ = &mut timeout => {
                    if let Some(status) = running.child.try_wait()? {
                        let output = finish_agent_output(status, running).await?;
                        return Ok(AgentRunResult::Completed(output));
                    }
                    let status = kill_and_wait(&mut running.child).await?;
                    finish_agent_output(status, running).await?;
                    return Err(timed_out_error());
                }
                changed = cancel_rx_ref.changed() => {
                    match changed {
                        Ok(()) if *cancel_rx_ref.borrow_and_update() => {
                            if let Some(status) = running.child.try_wait()? {
                                let output = finish_agent_output(status, running).await?;
                                return Ok(AgentRunResult::Completed(output));
                            }
                            let status = kill_and_wait(&mut running.child).await?;
                            finish_agent_output(status, running).await?;
                            return Ok(AgentRunResult::Cancelled);
                        }
                        Err(_) => {
                            cancel_rx = None;
                        }
                        _ => {}
                    }
                }
            }
        } else {
            tokio::select! {
                status = running.child.wait() => {
                    let output = finish_agent_output(status?, running).await?;
                    return Ok(AgentRunResult::Completed(output));
                }
                _ = &mut timeout => {
                    if let Some(status) = running.child.try_wait()? {
                        let output = finish_agent_output(status, running).await?;
                        return Ok(AgentRunResult::Completed(output));
                    }
                    let status = kill_and_wait(&mut running.child).await?;
                    finish_agent_output(status, running).await?;
                    return Err(timed_out_error());
                }
            }
        }
    }
}

fn spawn_reader_task<R>(mut reader: R) -> JoinHandle<std::io::Result<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer).await?;
        Ok(buffer)
    })
}

async fn kill_and_wait(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    match child.start_kill() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {}
        Err(e) => return Err(e),
    }
    child.wait().await
}

async fn finish_agent_output(
    status: std::process::ExitStatus,
    running: RunningAgent,
) -> std::io::Result<std::process::Output> {
    let stdout = join_reader_task(running.stdout_handle, "stdout").await?;
    let stderr = join_reader_task(running.stderr_handle, "stderr").await?;

    if let Some(stdin_handle) = running.stdin_handle {
        match stdin_handle.await {
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
    }

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

async fn join_reader_task(
    handle: JoinHandle<std::io::Result<Vec<u8>>>,
    stream_name: &str,
) -> std::io::Result<Vec<u8>> {
    match handle.await {
        Ok(result) => result,
        Err(join_err) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("agent {} reader task panicked: {}", stream_name, join_err),
        )),
    }
}

fn timed_out_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "Agent timed out after {} seconds. The child process may be hung (network stall, API outage, or infinite tool-use loop).",
            AGENT_TIMEOUT_SECS
        ),
    )
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

    static CTRL: OnceLock<Regex> = OnceLock::new();
    let ctrl = CTRL.get_or_init(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap());
    ctrl.replace_all(&stripped, "").to_string()
}

/// Check if an I/O error is E2BIG (argument list too long).
pub fn is_arg_too_long(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ArgumentListTooLong
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[tokio::test]
    async fn cancelling_running_agent_prevents_late_write() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("late-write.txt");

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 2; printf late > \"$OUT_PATH\"")
            .env("OUT_PATH", &output_path);

        let running = spawn_command(command, None).unwrap();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let handle =
            tokio::spawn(async move { run_running_agent(running, Some(&mut cancel_rx)).await });

        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_tx.send(true).unwrap();

        let result = handle.await.unwrap().unwrap();
        assert!(matches!(result, AgentRunResult::Cancelled));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!output_path.exists());
    }

    #[test]
    fn detects_rate_limit_text() {
        assert!(is_rate_limited_text("HTTP 429: Too Many Requests"));
        assert!(is_rate_limited_text("resource_exhausted, try again later"));
        assert!(!is_rate_limited_text("syntax error"));
    }

    #[test]
    fn extracts_retry_delay_from_phrases() {
        assert_eq!(
            extract_retry_delay("Rate limit hit. Retry after 90 seconds.").map(|d| d.as_secs()),
            Some(90)
        );
        assert_eq!(
            extract_retry_delay("Too many requests. Try again in 2 minutes.").map(|d| d.as_secs()),
            Some(120)
        );
    }

    #[test]
    fn extracts_retry_delay_from_generic_json_style_values() {
        assert_eq!(
            extract_retry_delay("{\"error\":\"429\",\"retryDelay\":\"42s\"}").map(|d| d.as_secs()),
            Some(42)
        );
    }
}
