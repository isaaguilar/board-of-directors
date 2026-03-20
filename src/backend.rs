use crate::claude_cli;
use crate::config::Backend;
use crate::copilot_cli;
use crate::gemini_cli;
use regex::Regex;
use std::env;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Null device path, platform-specific. Used to override git config paths.
pub const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
const NODE_HEAP_LIMIT_MB: &str = "8192";

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

/// Default agent timeout: 30 minutes.
///
/// Review, consolidation, and fix runs can spend several minutes in cold-start
/// work such as provider-side queueing or Gemini sandbox image pulls. A longer
/// timeout avoids failing healthy-but-slow agents during `bugfix`.
const AGENT_TIMEOUT_SECS: u64 = 1800;
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
    working_dir: &Path,
    allow_repo_access: bool,
    use_sandbox: bool,
    repo_root: &Path,
    state_dir: &Path,
) -> std::io::Result<std::process::Output> {
    match run_agent_inner_with_cancel(
        backend,
        prompt,
        model,
        working_dir,
        allow_repo_access,
        use_sandbox,
        repo_root,
        state_dir,
        None,
    )
    .await?
    {
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
    working_dir: &Path,
    allow_repo_access: bool,
    use_sandbox: bool,
    repo_root: &Path,
    state_dir: &Path,
    cancel_rx: &mut watch::Receiver<bool>,
) -> std::io::Result<AgentRunResult> {
    run_agent_inner_with_cancel(
        backend,
        prompt,
        model,
        working_dir,
        allow_repo_access,
        use_sandbox,
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
    working_dir: &Path,
    allow_repo_access: bool,
    use_sandbox: bool,
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
            working_dir,
            allow_repo_access,
            use_sandbox,
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
            let msg = format!(
                "Rate limit persisted after {} retries for backend '{}' and model '{}'. Giving up.",
                RATE_LIMIT_MAX_RETRIES, backend, model
            );
            eprintln!("{}", msg);
            return Err(std::io::Error::new(std::io::ErrorKind::Other, msg));
        }

        let wait = if let Some(dur) = retry_delay_from_output(&output) {
            dur
        } else {
            // Bounds-safe fallback indexing.
            let fallback_secs = RATE_LIMIT_FALLBACK_DELAYS_SECS
                .get(retry_count)
                .copied()
                .unwrap_or_else(|| *RATE_LIMIT_FALLBACK_DELAYS_SECS.last().unwrap());
            eprintln!(
                "Failed to parse retry delay from agent output; using fallback {}s. Raw output:\nSTDOUT:\n{}\nSTDERR:\n{}",
                fallback_secs,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Duration::from_secs(fallback_secs)
        };
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
    working_dir: &Path,
    allow_repo_access: bool,
    use_sandbox: bool,
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
            let cmd = copilot_cli::command(
                prompt,
                model,
                working_dir,
                allow_repo_access,
                repo_root,
                state_dir,
            )?;
            spawn_command(cmd, None)?
        }
        Backend::ClaudeCode => {
            let cmd = claude_cli::command(
                model,
                working_dir,
                allow_repo_access,
                repo_root,
                state_dir,
            )?;
            spawn_command(cmd, Some(prompt.as_bytes().to_vec()))?
        }
        Backend::GeminiCli => {
            let cmd = gemini_cli::command(
                model,
                working_dir,
                allow_repo_access,
                use_sandbox,
                repo_root,
                state_dir,
            )?;
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
    // Use regex-driven detection with word boundaries and common JSON/header forms
    static RATE_RE: OnceLock<Regex> = OnceLock::new();
    let re = RATE_RE.get_or_init(|| {
        Regex::new(r##"(?i)(?:(?:^|[^0-9.])429(?:$|[^0-9.])|http\s*/?\s*429|status\s*:\s*429|error\s*:\s*"?429(?:$|[^0-9.])|retry-after\s*:|\bretry_after\b|\btoo many requests\b|\brate limit\b|\brate-limit\b|\brate_limited\b|\bresource exhausted\b|\bresource_exhausted\b|\bquota exceeded\b)"##).unwrap()
    });
    re.is_match(text)
}

fn retry_delay_from_output(output: &std::process::Output) -> Option<Duration> {
    extract_retry_delay(&rate_limit_text(output))
}

fn extract_retry_delay(text: &str) -> Option<Duration> {
    // Prefer structured headers like "Retry-After: <seconds>"
    static RETRY_AFTER_RE: OnceLock<Regex> = OnceLock::new();
    let retry_after_re = RETRY_AFTER_RE.get_or_init(|| Regex::new(r"(?i)retry-after\s*:\s*(\d+)").unwrap());
    if let Some(caps) = retry_after_re.captures(text) {
        if let Ok(secs) = caps[1].parse::<u64>() {
            return Some(Duration::from_secs(secs.max(1)));
        }
    }

    // JSON-style fields: retry_after, retryAfter, retryDelay (e.g., "42s" or numeric)
    // Use a two-step, forgiving parse to avoid brittle single-regex failures.
    let key_re = Regex::new(r"(?i)(?:retry_after|retryafter|retrydelay|retry-after)").unwrap();
    if let Some(m) = key_re.find(text) {
        let suffix = &text[m.end()..];
        let val_re = Regex::new(r##"(?i)[:=]\s*\"?(\d+(?:\.\d+)?)(s|sec|secs|seconds|m|min|mins|minutes)?\"?"##).unwrap();
        if let Some(caps) = val_re.captures(suffix) {
            let val = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let unit = caps.get(2).map(|m| m.as_str()).unwrap_or("s");
            return duration_from_capture(val, unit);
        }
    }
    // Phrase-based detection with stricter boundaries.
    static PHRASE_RE: OnceLock<Regex> = OnceLock::new();
    let phrase_re = PHRASE_RE.get_or_init(|| Regex::new(r##"(?i)\b(?:retry(?: after| in)?|try again in|wait(?: for)?|available in|reset in)\b[^0-9]{0,20}\b(\d+(?:\.\d+)?)\b\s*(seconds?|secs?|s|minutes?|mins?|m)\b"##).unwrap());
    if let Some(caps) = phrase_re.captures(text) {
        return duration_from_capture(caps.get(1).map(|m| m.as_str())?, caps.get(2).map(|m| m.as_str()).unwrap_or("s"));
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

pub fn apply_node_heap_limit(command: &mut tokio::process::Command) {
    let existing = env::var("NODE_OPTIONS").ok();
    let combined = merge_node_options(existing.as_deref(), NODE_HEAP_LIMIT_MB);
    command.env("NODE_OPTIONS", combined);
}

fn merge_node_options(existing: Option<&str>, heap_limit_mb: &str) -> String {
    let heap_flag = format!("--max-old-space-size={heap_limit_mb}");
    match existing {
        Some(existing) if existing.contains("--max-old-space-size=") => existing.to_string(),
        Some(existing) if existing.trim().is_empty() => heap_flag,
        Some(existing) => format!("{existing} {heap_flag}"),
        None => heap_flag,
    }
}

/// Sanitize the child process environment for runs that must not access the
/// repository or invoke git. This unsets common git-related environment
/// variables and constructs a curated, persistent safe-PATH that uses
/// symlinks or small wrapper shims instead of copying full binaries per-run.
///
/// Behavior:
/// - If allow_repo_access is true, no changes are made.
/// - Attempts to create or reuse a persistent safe-bin under XDG_CONFIG_HOME
///   (or HOME/.config/board-of-directors/safe-bin). If that is not writable
///   the system temp dir fallback is used.
/// - For each allowed helper binary found on PATH, a symlink is created in the
///   safe-bin pointing to the original binary on Unix. On platforms without
///   reliable symlink support (Windows), a small wrapper batch file is created
///   that forwards arguments to the real binary. Existing entries are updated
///   if they point to a different target. This avoids copying large binaries
///   per-run and ensures predictable PATH contents.
/// - If no allowed binaries (including the program itself) can be located,
///   an error is returned.
pub fn sanitize_command_env(cmd: &mut tokio::process::Command, allow_repo_access: bool, program_name: &str) -> std::io::Result<()> {
    use std::io;
    use std::fs::OpenOptions;
    use fs2::FileExt;
    if allow_repo_access {
        return Ok(());
    }

    // Unset common git-related env vars.
    let git_envs = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_SSH",
        "GIT_TERMINAL_PROMPT",
        "GIT_SSL_NO_VERIFY",
    ];
    for key in git_envs.iter() {
        cmd.env_remove(key);
    }

    // Allowed helper binaries. The program_name is appended to ensure it is
    // resolvable when PATH is replaced.
    // NOTE: do NOT include shells or `env` here. Allowing shells (sh, bash)
    // enables shell-driven bypasses such as `sh -c "/usr/bin/git ..."` which
    // would defeat PATH-only sanitization. Absolute-path programs are refused
    // below.
    let mut allowed_bins = vec!["printf", "sleep", "cat", "sed", "awk", "grep", "xargs", "node", "python3", "python"];
    if !allowed_bins.contains(&program_name) {
        allowed_bins.push(program_name);
    }
    allowed_bins.sort();
    allowed_bins.dedup();

    // Reject program names that look like absolute paths or shells. Allowing
    // shells (sh/bash) or raw absolute paths would let an agent execute
    // absolute-path binaries such as /usr/bin/git and bypass PATH-based
    // sanitization.
    if program_name.contains('/') || program_name.contains('\\')
        || program_name.eq_ignore_ascii_case("sh")
        || program_name.eq_ignore_ascii_case("bash")
        || program_name.eq_ignore_ascii_case("env")
    {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("sanitize_command_env: refusing to allow shell or absolute-path program '{}'", program_name),
        ));
    }

    // Helper to find an executable in a directory with PATHEXT awareness on Windows.
    fn find_in_dir(bin: &str, dir: &std::path::Path) -> Option<std::path::PathBuf> {
        #[cfg(windows)]
        {
            let pathext = std::env::var_os("PATHEXT").map(|s| s.into_string().unwrap_or_default()).unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".to_string());
            let exts: Vec<&str> = pathext.split(';').filter_map(|s| {
                let s = s.trim();
                if s.is_empty() { None } else { Some(s) }
            }).collect();
            // Try the name as-is first, then with extensions.
            let candidate = dir.join(bin);
            if candidate.exists() { return Some(candidate); }
            for ext in exts {
                // ext may include leading dot
                let candidate = dir.join(format!("{}{}", bin, ext));
                if candidate.exists() { return Some(candidate); }
            }
            None
        }
        #[cfg(not(windows))]
        {
            let candidate = dir.join(bin);
            if candidate.exists() { Some(candidate) } else { None }
        }
    }

    // Find candidate sources for each allowed binary from the existing PATH,
    // skipping any candidate that is the git executable. PATHEXT-aware on Windows.
    let path_os = std::env::var_os("PATH").unwrap_or_default();
    let git_name = if cfg!(windows) { "git" } else { "git" };
    let mut sources: Vec<(String, std::path::PathBuf)> = Vec::new();
    for bin in allowed_bins.iter() {
        if bin.eq_ignore_ascii_case(&git_name) {
            continue;
        }
        let mut found = None;
        for p in std::env::split_paths(&path_os) {
            if let Some(src) = find_in_dir(bin, &p) {
                // Avoid selecting git even if it's named differently (git.exe etc).
                if let Some(fname) = src.file_name().and_then(|s| s.to_str()) {
                    if fname.to_ascii_lowercase().starts_with("git") {
                        continue;
                    }
                }
                found = Some(src);
                break;
            }
        }
        if let Some(src) = found {
            sources.push((bin.to_string(), src));
        }
    }

    if sources.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "sanitize_command_env: unable to construct a minimal safe PATH; no allowed binaries found. Refusing to run to avoid exposing git."
        ));
    }

    // Determine persistent safe-dir location. Prefer XDG_CONFIG_HOME or
    // HOME/.config/board-of-directors/safe-bin. Fallback to system temp dir.
    let safe_dir = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(xdg).join("board-of-directors").join("safe-bin")
    } else if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home).join(".config").join("board-of-directors").join("safe-bin")
    } else {
        std::env::temp_dir().join("bod-safe-bin-global")
    };

    // Populate the safe-dir while avoiding blocking Tokio worker threads.
    // If running inside a Tokio runtime, use block_in_place to run the
    // blocking population on the blocking thread pool. Otherwise run the
    // blocking population inline.
    // Helper that performs the blocking population work given owned inputs.
    fn populate_inner(safe_dir: &std::path::PathBuf, sources: &Vec<(String, std::path::PathBuf)>) -> io::Result<()> {
        // Create parent dirs and acquire a file lock to avoid races when multiple
        // processes populate or refresh the safe-dir concurrently.
        std::fs::create_dir_all(safe_dir)?;
        let lock_path = safe_dir.join(".populate.lock");
        use std::fs::OpenOptions;
        let lock_file = OpenOptions::new().create(true).read(true).write(true).open(&lock_path)?;
        // Block until we can acquire an exclusive lock. This prevents concurrent
        // processes from modifying the same destination files and avoids races.
        lock_file.lock_exclusive()?;

        // Ensure we release the lock at the end of the scope.
        let lock_guard = scopeguard::guard(lock_file, |f| {
            let _ = f.unlock();
        });

        // For each source binary, ensure a symlink or shim exists at safe_dir/name.
        for (name, src) in sources.iter() {
            let dest = safe_dir.join(name);
            // If dest exists, check if it already points to same target. If not,
            // replace it.
            if dest.exists() {
                // On Unix, prefer checking symlink target. On Windows, compare file
                // metadata when possible.
                let need_replace = match std::fs::read_link(&dest) {
                    Ok(target) => target != *src,
                    Err(_) => {
                        match (std::fs::metadata(&dest), std::fs::metadata(&src)) {
                            (Ok(md1), Ok(md2)) => md1.len() != md2.len() || md1.permissions().readonly() != md2.permissions().readonly(),
                            _ => true,
                        }
                    }
                };
                if !need_replace {
                    continue;
                }
            }

            // Create a temporary path for atomic replace.
            let tmp_name = format!("{}.tmp.{}", name, std::process::id());
            let tmp_dest = safe_dir.join(&tmp_name);

            // Remove any leftover tmp file first (best-effort).
            let _ = std::fs::remove_file(&tmp_dest);

            // Try to create a symlink first on Unix-like platforms. Write to tmp
            // path and then atomically rename into place to avoid races.
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                let res = (|| -> io::Result<()> {
                    symlink(src, &tmp_dest)?;
                    std::fs::rename(&tmp_dest, &dest)?;
                    Ok(())
                })();
                if let Err(e) = res {
                    eprintln!("sanitize_command_env: symlink failed for {} -> {}: {}; creating shim", src.display(), dest.display(), e);
                    let shim = format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", src.display());
                    std::fs::write(&tmp_dest, shim.as_bytes())?;
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(&tmp_dest)?.permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(&tmp_dest, perms)?;
                    std::fs::rename(&tmp_dest, &dest)?;
                }
            }

            #[cfg(windows)]
            {
                // On Windows create a .bat wrapper that forwards all args.
                // Example content: @"C:\path\to\bin.exe" %*
                let shim_path = tmp_dest.with_extension("bat");
                let content = format!("@\"{}\" %*\r\n", src.display());
                std::fs::write(&shim_path, content.as_bytes())?;
                // Atomically move to final location name.bat
                let final_path = dest.with_extension("bat");
                let _ = std::fs::remove_file(&final_path);
                std::fs::rename(&shim_path, &final_path)?;
            }
        }

        // Unlock happens via lock_guard drop here.
        drop(lock_guard);

        Ok(())
    }

    let safe_dir_clone = safe_dir.clone();
    let sources_clone = sources.clone();

    // Run population off the current thread to avoid panics on current-thread Tokio runtimes.
    // Use a dedicated std thread so this function remains synchronous while avoiding blocking
    // Tokio worker threads. If spawning the thread fails, fall back to inline population.
    if tokio::runtime::Handle::try_current().is_ok() {
        let safe_dir_for_thread = safe_dir_clone.clone();
        let sources_for_thread = sources_clone.clone();
        let handle = std::thread::Builder::new()
            .name("bod-safe-bin-populate".to_string())
            .spawn(move || populate_inner(&safe_dir_for_thread, &sources_for_thread));
        match handle {
            Ok(join_handle) => {
                match join_handle.join() {
                    Ok(res) => res?,
                    Err(join_err) => {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("sanitize_command_env: populate thread panicked: {:?}", join_err),
                        ));
                    }
                }
            }
            Err(e) => {
                eprintln!("sanitize_command_env: failed to spawn populate thread: {}; falling back to inline populate", e);
                populate_inner(&safe_dir_clone, &sources_clone)?;
            }
        }
    } else {
        populate_inner(&safe_dir, &sources)?;
    }

    // Final check: ensure the program binary (or its shim) exists in safe_dir.
    let mut prog_path = safe_dir_clone.join(program_name);
    if cfg!(windows) {
        if !prog_path.exists() {
            // Check PATHEXT-aware variants, including .bat created shims.
            let pathext = std::env::var_os("PATHEXT").map(|s| s.into_string().unwrap_or_default()).unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".to_string());
            for ext in pathext.split(';').filter_map(|s| {
                let s = s.trim(); if s.is_empty() { None } else { Some(s) }
            }) {
                let candidate = safe_dir_clone.join(format!("{}{}", program_name, ext));
                if candidate.exists() {
                    prog_path = candidate;
                    break;
                }
            }
            // Also check .bat shim
            let bat = safe_dir_clone.join(program_name).with_extension("bat");
            if prog_path.as_path().exists() == false && bat.exists() {
                prog_path = bat;
            }
        }
    }
    if !prog_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("sanitize_command_env: program '{}' was not found in the constructed safe PATH; refusing to run.", program_name)
        ));
    }

    // Warn: PATH-based sanitization reduces risk but cannot stop a child process
    // from executing an absolute-path binary (for example, "/usr/bin/git"). Recommend
    // OS-level sandboxing (containers, seccomp, mount namespaces) for stronger isolation.
    eprintln!("sanitize_command_env: WARNING: PATH-based sanitization is not a full sandbox. Child processes may still exec absolute-path binaries and bypass PATH shims. Use OS-level sandboxing (containers, seccomp, namespaces) or run agents in restricted environments for stronger isolation.");
    // Warn: PATH-based sanitization reduces risk but cannot stop a child process
    // from executing an absolute-path binary (for example, "/usr/bin/git"). Recommend
    // OS-level sandboxing (containers, seccomp, mount namespaces) for stronger isolation.
    eprintln!("sanitize_command_env: WARNING: PATH-based sanitization is not a full sandbox. Child processes may still exec absolute-path binaries and bypass PATH shims. Use OS-level sandboxing (containers, seccomp, namespaces) or run agents in restricted environments for stronger isolation.");
    // Set PATH to the single safe-dir.
    cmd.env("PATH", safe_dir_clone.to_str().unwrap_or_default());
    Ok(())
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
        // Ensure "429" embedded in other tokens does not spuriously match
        assert!(!is_rate_limited_text("version 1.429.0"));
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

    #[test]
    fn parses_retry_after_header() {
        let hdr = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 10\r\n";
        assert_eq!(extract_retry_delay(hdr).map(|d| d.as_secs()), Some(10));
    }

    #[test]
    fn merge_node_options_appends_heap_limit_when_missing() {
        assert_eq!(
            merge_node_options(Some("--trace-warnings"), "8192"),
            "--trace-warnings --max-old-space-size=8192"
        );
    }

    #[test]
    fn merge_node_options_preserves_existing_heap_limit() {
        assert_eq!(
            merge_node_options(Some("--max-old-space-size=4096 --trace-warnings"), "8192"),
            "--max-old-space-size=4096 --trace-warnings"
        );
    }
}
