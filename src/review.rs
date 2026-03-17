use crate::agents;
use crate::backend;
use crate::config::{Backend, Config};
use crate::files;
use crate::git;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewErrorKind {
    FatalSetup,
    Retryable,
}

#[derive(Debug, Clone)]
pub struct ReviewError {
    kind: ReviewErrorKind,
    message: String,
    /// When some agents succeed and others fail, the timestamp of the
    /// partial review round is preserved so callers can still consolidate
    /// the successful reviews.
    pub timestamp: Option<String>,
}

impl ReviewError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            kind: ReviewErrorKind::FatalSetup,
            message: message.into(),
            timestamp: None,
        }
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self {
            kind: ReviewErrorKind::Retryable,
            message: message.into(),
            timestamp: None,
        }
    }

    fn retryable_with_timestamp(message: impl Into<String>, timestamp: String) -> Self {
        Self {
            kind: ReviewErrorKind::Retryable,
            message: message.into(),
            timestamp: Some(timestamp),
        }
    }

    pub fn is_fatal(&self) -> bool {
        self.kind == ReviewErrorKind::FatalSetup
    }
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub async fn run(config: &Config) -> Result<String, ReviewError> {
    let repo_root = git::repo_root().map_err(ReviewError::fatal)?;
    let state_dir = files::ensure_state_dir(&repo_root).map_err(ReviewError::fatal)?;

    let default_branch = git::detect_default_branch().map_err(ReviewError::fatal)?;
    let branch = git::current_branch().map_err(ReviewError::fatal)?;
    let sanitized = agents::sanitize_branch_name(&branch).ok_or_else(|| {
        ReviewError::fatal(format!(
            "Branch name '{}' contains no alphanumeric characters and cannot be used \
             for review filenames. Please use a branch with at least one alphanumeric character.",
            branch
        ))
    })?;
    let timestamp = agents::timestamp_now();

    println!(
        "Reviewing branch '{}' against '{}'...",
        branch, default_branch
    );

    let diff = git::generate_diff(&default_branch).map_err(ReviewError::fatal)?;

    // Truncate diff if extremely large to avoid overwhelming agents
    let max_diff_len = 100_000;
    let truncated_diff: String;
    let diff_for_prompt = if diff.len() > max_diff_len {
        println!(
            "Warning: diff is large ({} bytes), truncating to {} bytes for review.",
            diff.len(),
            max_diff_len
        );
        truncated_diff = format!(
            "{}\n\n... [diff truncated at {} bytes; original size {} bytes] ...",
            &diff[..diff.floor_char_boundary(max_diff_len)],
            max_diff_len,
            diff.len()
        );
        truncated_diff.as_str()
    } else {
        &diff
    };

    // Reserve all output files first (before spawning any tasks) so that a
    // file-creation failure doesn't leave already-spawned tasks running as
    // detached zombies. Tokio tasks are NOT cancelled when a JoinHandle is dropped.
    let mut reserved: Vec<(String, PathBuf, agents::ReservedFile, String, String)> = Vec::new();
    for entry in &config.review.models {
        let (filename, guard) = agents::create_review_file(
            &state_dir, &entry.codename, &sanitized, &timestamp,
        )
        .map_err(|e| ReviewError::fatal(format!("Failed to reserve review file: {}", e)))?;
        let output_path = state_dir.join(&filename);
        reserved.push((
            filename,
            output_path,
            guard,
            entry.model.clone(),
            entry.codename.clone(),
        ));
    }

    let mut handles = Vec::new();

    for (filename, output_path, guard, model_id, codename) in reserved {
        let diff_text = diff_for_prompt.to_string();
        let repo_root = repo_root.clone();
        let state_dir = state_dir.clone();
        let backend = config.backend;

        let handle = tokio::spawn(async move {
            run_agent_review(
                &backend, &repo_root, &state_dir, &codename, &model_id, &diff_text, &output_path,
                guard,
            )
            .await
        });

        handles.push((filename, handle));
    }

    let mut success_count = 0;
    let mut fail_count = 0;
    let mut has_fatal = false;
    let mut fatal_message = String::new();

    for (filename, handle) in handles {
        match handle.await {
            Ok(Ok(())) => {
                println!("  [ok] {}", filename);
                success_count += 1;
            }
            Ok(Err(e)) => {
                eprintln!("  [FAIL] {}: {}", filename, e);
                if e.is_fatal() && !has_fatal {
                    has_fatal = true;
                    fatal_message = e.to_string();
                }
                fail_count += 1;
            }
            Err(e) => {
                eprintln!("  [FAIL] {}: task panicked: {}", filename, e);
                fail_count += 1;
            }
        }
    }

    println!(
        "\nReview complete: {} succeeded, {} failed.",
        success_count, fail_count
    );

    if fail_count > 0 {
        if has_fatal {
            return Err(ReviewError::fatal(fatal_message));
        }
        if success_count > 0 {
            // Partial success: some reviews were written. Preserve the timestamp
            // so callers can consolidate the successful reviews.
            Err(ReviewError::retryable_with_timestamp(
                format!(
                    "{} of {} agent(s) failed.",
                    fail_count,
                    fail_count + success_count
                ),
                timestamp,
            ))
        } else {
            Err(ReviewError::retryable(format!(
                "{} agent(s) failed.",
                fail_count
            )))
        }
    } else {
        Ok(timestamp)
    }
}

async fn run_agent_review(
    backend: &Backend,
    repo_root: &Path,
    state_dir: &Path,
    codename: &str,
    model_id: &str,
    diff: &str,
    output_path: &PathBuf,
    mut guard: agents::ReservedFile,
) -> Result<(), ReviewError> {
    let output_path_str = output_path.to_string_lossy().to_string();

    let prompt = format!(
        r#"You are a senior code reviewer. Review the following git diff for a pull request.

Your task:
- Identify critical bugs, logic errors, security vulnerabilities, and correctness issues.
- Be very critical but constructive -- provide actionable feedback.
- Prioritize correctness over complexity.
- Keep your review concise enough for a human to read quickly. Do not be overly verbose.
- Format your review as markdown.
        - Do NOT run `git commit` or `git push`.
        - Read-only git commands for research are allowed when helpful (for example `git status`, `git diff`, `git log`, and `git show`).
        - Avoid any git command that changes the checked-out branch, commit history, index, or working tree unless it is strictly temporary research and you restore the branch to exactly the same uncommitted state and history it had before.
        - Treat files under `~/.config/board-of-directors/` as internal board-of-directors tooling state.
        - Do NOT inspect, reference, or use that tooling state as evidence about repository correctness.
- Do NOT reference other reviewers or reviews.
- The only allowed interaction with tooling state is writing the review file requested below.

Write your complete review to the file: {output_path_str}

Here is the diff to review:

```diff
{diff}
```"#
    );

    let output = backend::run_agent(backend, &prompt, model_id, repo_root, state_dir)
        .await
        .map_err(|e| {
            if backend::is_arg_too_long(&e) {
                ReviewError::fatal(
                    "Prompt exceeds OS argument-size limit (E2BIG). \
                     The diff may be too large for command-line passing. \
                     Consider reviewing a smaller changeset."
                )
            } else {
                ReviewError::retryable(format!("{} failed to start agent: {}", codename, e))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ReviewError::retryable(format!(
            "{} agent exited with error: {}",
            codename, stderr
        )));
    }

    // Check file size, not existence, because create_review_file() pre-creates
    // a 0-byte file to atomically reserve the filename. A 0-byte file means the
    // agent did not write via tool use and we should fall back to stdout.
    let meta = tokio::fs::metadata(&output_path).await.ok();
    let file_is_empty = meta.map_or(true, |m| m.len() == 0);
    if file_is_empty {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let clean = backend::strip_ansi_codes(&stdout);
        if clean.trim().is_empty() {
            return Err(ReviewError::retryable(format!(
                "{} produced no output and did not create file",
                codename
            )));
        }
        tokio::fs::write(output_path, clean.as_bytes())
            .await
            .map_err(|e| {
                ReviewError::retryable(format!("{} failed to write review file: {}", codename, e))
            })?;
    } else {
        // Agent wrote the file via tool use -- strip ANSI codes in case the agent
        // included terminal escape sequences (e.g. from copied command output).
        // Disarm the guard first: the file has valid content regardless of whether
        // the optional ANSI cleanup succeeds.
        guard.disarm();
        let raw = tokio::fs::read_to_string(output_path)
            .await
            .map_err(|e| {
                ReviewError::retryable(format!("{} failed to read review file: {}", codename, e))
            })?;
        let clean = backend::strip_ansi_codes(&raw);
        if clean != raw {
            if let Err(e) = tokio::fs::write(output_path, clean.as_bytes()).await {
                eprintln!(
                    "Warning: {} failed to re-write cleaned review file: {}. \
                     Keeping original content.",
                    codename, e
                );
            }
        }
    }

    // Guard may already be disarmed (agent-written path above). Calling disarm
    // again on an already-disarmed guard is a no-op.
    guard.disarm();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_review_error_is_marked_fatal() {
        let error = ReviewError::fatal("fatal");
        assert!(error.is_fatal());
        assert_eq!(error.to_string(), "fatal");
    }

    #[test]
    fn retryable_review_error_is_not_fatal() {
        let error = ReviewError::retryable("retry");
        assert!(!error.is_fatal());
        assert_eq!(error.to_string(), "retry");
    }
}
