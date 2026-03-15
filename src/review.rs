use crate::agents;
use crate::config::Config;
use crate::copilot_cli;
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
}

impl ReviewError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            kind: ReviewErrorKind::FatalSetup,
            message: message.into(),
        }
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self {
            kind: ReviewErrorKind::Retryable,
            message: message.into(),
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

pub async fn run(config: &Config) -> Result<(), ReviewError> {
    let repo_root = git::repo_root().map_err(ReviewError::fatal)?;
    let state_dir = files::ensure_state_dir(&repo_root).map_err(ReviewError::fatal)?;

    let default_branch = git::detect_default_branch().map_err(ReviewError::fatal)?;
    let branch = git::current_branch().map_err(ReviewError::fatal)?;
    let sanitized = agents::sanitize_branch_name(&branch);

    println!(
        "Reviewing branch '{}' against '{}'...",
        branch, default_branch
    );

    let diff = git::generate_diff(&default_branch).map_err(ReviewError::fatal)?;

    // Truncate diff if extremely large to avoid overwhelming agents
    let max_diff_len = 100_000;
    let diff_for_prompt = if diff.len() > max_diff_len {
        println!(
            "Warning: diff is large ({} bytes), truncating to {} bytes for review.",
            diff.len(),
            max_diff_len
        );
        &diff[..diff.floor_char_boundary(max_diff_len)]
    } else {
        &diff
    };

    let mut handles = Vec::new();

    for entry in &config.review.models {
        let review_num = agents::next_review_number(&state_dir, &entry.codename, &sanitized);
        let filename = agents::review_filename(&entry.codename, &sanitized, review_num);
        let output_path = state_dir.join(&filename);
        let model_id = entry.model.clone();
        let codename = entry.codename.clone();
        let diff_text = diff_for_prompt.to_string();
        let out_path = output_path.clone();
        let repo_root = repo_root.clone();
        let state_dir = state_dir.clone();

        let handle = tokio::spawn(async move {
            run_agent_review(
                &repo_root, &state_dir, &codename, &model_id, &diff_text, &out_path,
            )
            .await
        });

        handles.push((filename, handle));
    }

    let mut success_count = 0;
    let mut fail_count = 0;

    for (filename, handle) in handles {
        match handle.await {
            Ok(Ok(())) => {
                println!("  [ok] {}", filename);
                success_count += 1;
            }
            Ok(Err(e)) => {
                eprintln!("  [FAIL] {}: {}", filename, e);
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
        Err(ReviewError::retryable(format!(
            "{} agent(s) failed.",
            fail_count
        )))
    } else {
        Ok(())
    }
}

async fn run_agent_review(
    repo_root: &Path,
    state_dir: &Path,
    codename: &str,
    model_id: &str,
    diff: &str,
    output_path: &PathBuf,
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
- Never run git commands. Never create commits. Never push.
- Treat all board-of-directors generated state as internal tooling artifacts, including any legacy `.bod*` repo files and files under `~/.config/board-of-directors/`.
- Do NOT inspect, reference, or use that tooling state as evidence about repository correctness.
- Do NOT reference other reviewers or reviews.
- The only allowed interaction with tooling state is writing the review file requested below.

Write your complete review to the file: {output_path_str}

Here is the diff to review:

```diff
{diff}
```"#
    );

    let output = copilot_cli::command(&prompt, model_id, repo_root, state_dir)
        .output()
        .await
        .map_err(|e| {
            ReviewError::retryable(format!("{} failed to start copilot: {}", codename, e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ReviewError::retryable(format!(
            "{} copilot exited with error: {}",
            codename, stderr
        )));
    }

    // If copilot didn't write the file via tool use, write stdout as fallback
    if !output_path.exists() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Err(ReviewError::retryable(format!(
                "{} produced no output and did not create file",
                codename
            )));
        }
        tokio::fs::write(output_path, stdout.as_bytes())
            .await
            .map_err(|e| {
                ReviewError::retryable(format!("{} failed to write review file: {}", codename, e))
            })?;
    }

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
