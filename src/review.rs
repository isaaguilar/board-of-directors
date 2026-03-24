use crate::agents;
use crate::backend;
use crate::bugfix_session::BugfixSession;
use crate::config::{Backend, Config};
use crate::files;
use crate::git;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewAgentRequest {
    prompt: String,
    working_dir: PathBuf,
    allow_repo_access: bool,
    use_sandbox: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewContextArtifacts {
    default_branch: String,
    full_diff_path: PathBuf,
    diff_stat_path: PathBuf,
    changed_files_path: PathBuf,
    changed_file_count: usize,
    diff_bytes: usize,
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

    // Reserve all output files first (before spawning any tasks) so that a
    // file-creation failure doesn't leave already-spawned tasks running as
    // detached zombies. Tokio tasks are NOT cancelled when a JoinHandle is dropped.
    let mut reserved: Vec<(
        String,
        PathBuf,
        agents::ReservedFile,
        Backend,
        String,
        String,
    )> = Vec::new();
    for entry in &config.review.models {
        let (filename, guard) =
            agents::create_review_file(&state_dir, &entry.codename, &sanitized, &timestamp)
                .map_err(|e| ReviewError::fatal(format!("Failed to reserve review file: {}", e)))?;
        let output_path = state_dir.join(&filename);
        reserved.push((
            filename,
            output_path,
            guard,
            entry.backend,
            entry.model.clone(),
            entry.codename.clone(),
        ));
    }

    let review_context =
        generate_review_context_artifacts(&state_dir, &sanitized, &timestamp, &default_branch)?;
    println!(
        "  Review context prepared: {} changed file(s), {} bytes of diff.",
        review_context.changed_file_count, review_context.diff_bytes
    );

    let start_delays = reviewer_start_delays(reserved.len());
    let mut handles = Vec::new();

    for ((filename, output_path, guard, agent_backend, model_id, codename), start_delay) in
        reserved.into_iter().zip(start_delays.into_iter())
    {
        let review_context = review_context.clone();
        let repo_root = repo_root.clone();
        let state_dir = state_dir.clone();
        if !start_delay.is_zero() {
            println!(
                "  Scheduling reviewer '{}' to start in {}s to spread agent load.",
                codename,
                start_delay.as_secs()
            );
        }

        let handle = tokio::spawn(async move {
            if !start_delay.is_zero() {
                tokio::time::sleep(start_delay).await;
            }
            run_agent_review(
                &agent_backend,
                &repo_root,
                &state_dir,
                &codename,
                &model_id,
                &review_context,
                &output_path,
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

pub async fn run_for_bugfix(
    config: &Config,
    session: &BugfixSession,
) -> Result<String, ReviewError> {
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
    session
        .begin_review(config.review.models.len() as u32)
        .await;

    let mut reserved: Vec<(
        String,
        PathBuf,
        agents::ReservedFile,
        Backend,
        String,
        String,
    )> = Vec::new();
    for entry in &config.review.models {
        let (filename, guard) =
            agents::create_review_file(&state_dir, &entry.codename, &sanitized, &timestamp)
                .map_err(|e| ReviewError::fatal(format!("Failed to reserve review file: {}", e)))?;
        let output_path = state_dir.join(&filename);
        reserved.push((
            filename,
            output_path,
            guard,
            entry.backend,
            entry.model.clone(),
            entry.codename.clone(),
        ));
    }

    let review_context =
        generate_review_context_artifacts(&state_dir, &sanitized, &timestamp, &default_branch)?;
    println!(
        "  Review context prepared: {} changed file(s), {} bytes of diff.",
        review_context.changed_file_count, review_context.diff_bytes
    );

    let mut join_set = tokio::task::JoinSet::new();
    let start_delays = reviewer_start_delays(reserved.len());
    for ((filename, output_path, guard, agent_backend, model_id, codename), start_delay) in
        reserved.into_iter().zip(start_delays.into_iter())
    {
        let review_context = review_context.clone();
        let repo_root = repo_root.clone();
        let state_dir = state_dir.clone();
        if !start_delay.is_zero() {
            println!(
                "  Scheduling reviewer '{}' to start in {}s to spread agent load.",
                codename,
                start_delay.as_secs()
            );
        }
        join_set.spawn(async move {
            if !start_delay.is_zero() {
                tokio::time::sleep(start_delay).await;
            }
            (
                filename,
                codename.clone(),
                run_agent_review(
                    &agent_backend,
                    &repo_root,
                    &state_dir,
                    &codename,
                    &model_id,
                    &review_context,
                    &output_path,
                    guard,
                )
                .await,
            )
        });
    }

    let mut cancel_rx = session.subscribe_cancel();
    let mut success_count = 0;
    let mut fail_count = 0;
    let mut has_fatal = false;
    let mut fatal_message = String::new();

    loop {
        let next = tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    join_set.abort_all();
                    session.set_message("Cancelling reviewer tasks...").await;
                    return Err(ReviewError::retryable("Review cancelled from the browser UI."));
                }
                continue;
            }
            joined = join_set.join_next() => joined,
        };

        let Some(joined) = next else {
            break;
        };

        match joined {
            Ok((filename, codename, Ok(()))) => {
                println!("  [ok] {}", filename);
                session
                    .note_review_agent_result(&codename, true, None)
                    .await;
                success_count += 1;
            }
            Ok((filename, codename, Err(e))) => {
                eprintln!("  [FAIL] {}: {}", filename, e);
                let reason = e.to_string();
                session
                    .note_review_agent_result(&codename, false, Some(&reason))
                    .await;
                if e.is_fatal() && !has_fatal {
                    has_fatal = true;
                    fatal_message = reason;
                }
                fail_count += 1;
            }
            Err(e) => {
                eprintln!("  [FAIL] reviewer task panicked or was aborted: {}", e);
                fail_count += 1;
            }
        }
    }

    println!(
        "\nReview complete: {} succeeded, {} failed.",
        success_count, fail_count
    );
    if success_count > 0 {
        session.finish_review_round(&timestamp).await;
    }

    if fail_count > 0 {
        if has_fatal {
            return Err(ReviewError::fatal(fatal_message));
        }
        if success_count > 0 {
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

fn generate_review_context_artifacts(
    state_dir: &Path,
    sanitized_branch: &str,
    timestamp: &str,
    default_branch: &str,
) -> Result<ReviewContextArtifacts, ReviewError> {
    let diff = git::generate_diff(default_branch).map_err(ReviewError::fatal)?;
    let diff_stat = git::generate_diff_stat(default_branch).map_err(ReviewError::fatal)?;
    let changed_files = git::generate_changed_files(default_branch).map_err(ReviewError::fatal)?;

    write_review_context_artifacts(
        state_dir,
        sanitized_branch,
        timestamp,
        default_branch,
        &diff,
        &diff_stat,
        &changed_files,
    )
}

fn write_review_context_artifacts(
    state_dir: &Path,
    sanitized_branch: &str,
    timestamp: &str,
    default_branch: &str,
    diff: &str,
    diff_stat: &str,
    changed_files: &[String],
) -> Result<ReviewContextArtifacts, ReviewError> {
    let (full_diff_path, diff_stat_path, changed_files_path) =
        build_review_context_paths(state_dir, sanitized_branch, timestamp);
    let changed_files_text = if changed_files.is_empty() {
        String::new()
    } else {
        format!("{}\n", changed_files.join("\n"))
    };
    let mut created = Vec::new();

    if let Err(error) = write_text_artifact(&full_diff_path, diff) {
        return Err(ReviewError::fatal(error));
    }
    created.push(full_diff_path.clone());

    if let Err(error) = write_text_artifact(&diff_stat_path, diff_stat) {
        cleanup_created_artifacts(&created);
        return Err(ReviewError::fatal(error));
    }
    created.push(diff_stat_path.clone());

    if let Err(error) = write_text_artifact(&changed_files_path, &changed_files_text) {
        cleanup_created_artifacts(&created);
        return Err(ReviewError::fatal(error));
    }

    Ok(ReviewContextArtifacts {
        default_branch: default_branch.to_string(),
        full_diff_path,
        diff_stat_path,
        changed_files_path,
        changed_file_count: changed_files.len(),
        diff_bytes: diff.len(),
    })
}

fn build_review_context_paths(
    state_dir: &Path,
    sanitized_branch: &str,
    timestamp: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    (
        state_dir.join(format!("{}-diff-{}.patch", timestamp, sanitized_branch)),
        state_dir.join(format!("{}-diffstat-{}.txt", timestamp, sanitized_branch)),
        state_dir.join(format!("{}-files-{}.txt", timestamp, sanitized_branch)),
    )
}

fn write_text_artifact(path: &Path, contents: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("Failed to create {}: {}", path.display(), e))?;
    if let Err(e) = file.write_all(contents.as_bytes()) {
        let _ = std::fs::remove_file(path);
        return Err(format!("Failed to write {}: {}", path.display(), e));
    }
    Ok(())
}

fn cleanup_created_artifacts(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

async fn run_agent_review(
    backend: &Backend,
    repo_root: &Path,
    state_dir: &Path,
    codename: &str,
    model_id: &str,
    review_context: &ReviewContextArtifacts,
    output_path: &PathBuf,
    mut guard: agents::ReservedFile,
) -> Result<(), ReviewError> {
    let request = build_review_agent_request(repo_root, state_dir, output_path, review_context);

    let output = backend::run_agent(
        backend,
        &request.prompt,
        model_id,
        &request.working_dir,
        request.allow_repo_access,
        request.use_sandbox,
        repo_root,
        state_dir,
    )
    .await
    .map_err(|e| {
        if backend::is_arg_too_long(&e) {
            ReviewError::fatal(
                "Prompt exceeds OS argument-size limit (E2BIG). \
                 Consider checking prompt construction or backend CLI limits.",
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
        let raw = tokio::fs::read_to_string(output_path).await.map_err(|e| {
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

fn build_review_agent_request(
    repo_root: &Path,
    state_dir: &Path,
    output_path: &Path,
    review_context: &ReviewContextArtifacts,
) -> ReviewAgentRequest {
    let output_path_str = output_path.to_string_lossy().to_string();
    let repo_root_str = repo_root.to_string_lossy().to_string();
    let full_diff_path_str = review_context.full_diff_path.to_string_lossy().to_string();
    let diff_stat_path_str = review_context.diff_stat_path.to_string_lossy().to_string();
    let changed_files_path_str = review_context
        .changed_files_path
        .to_string_lossy()
        .to_string();

    let prompt = format!(
        r#"You are a senior code reviewer. Review the current git branch against `origin/{default_branch}`.

Your task:
- Identify bugs, logic errors, security vulnerabilities, and correctness issues -- but only flag real problems that genuinely exist.
- Be objective and constructive -- provide actionable feedback calibrated to the actual severity of each issue.
- If the code is correct and the change is sound, say so. Do not invent or inflate issues to fill the review.
- Prioritize correctness over complexity.
- Keep your review concise enough for a human to read quickly. Do not be overly verbose.
- Format your review as markdown.
- Do NOT run `git commit` or `git push`.
- You may inspect repository files and use read-only git commands for research when helpful. Because your current working directory is tooling state outside the repository, run git commands as `git -C {repo_root_str} <args>` so they target the repository explicitly.
- Do NOT edit repository files, create files in the repository, or use write tools against repository paths.
- Your current working directory is board-of-directors tooling state outside the repository.
- Do NOT inspect, reference, or use that tooling state as evidence about repository correctness.
- Do NOT reference other reviewers or reviews.
- The only allowed interaction with tooling state is writing the review file requested below.
- The full diff file below is the source of truth for the entire change set. Review the full scope of changes, not just a subset.

Review context files in tooling state:
- Full diff: {full_diff_path_str}
- Diff stat: {diff_stat_path_str}
- Changed files: {changed_files_path_str}

Quick context:
- Changed files: {changed_file_count}
- Full diff size: {diff_bytes} bytes

Write your complete review to the file: {output_path_str}

Use the diff/context files above plus any read-only repo inspection you need to produce the review."#,
        default_branch = review_context.default_branch,
        changed_file_count = review_context.changed_file_count,
        diff_bytes = review_context.diff_bytes
    );

    ReviewAgentRequest {
        prompt,
        working_dir: state_dir.to_path_buf(),
        allow_repo_access: true,
        use_sandbox: false,
    }
}

fn reviewer_start_delays(count: usize) -> Vec<Duration> {
    let mut delays = Vec::with_capacity(count);
    let mut cumulative_secs = 0u64;
    for index in 0..count {
        if index > 0 {
            cumulative_secs += random_reviewer_jitter_secs();
        }
        delays.push(Duration::from_secs(cumulative_secs));
    }
    delays
}

fn random_reviewer_jitter_secs() -> u64 {
    let mut bytes = [0u8; 1];
    if let Err(e) = getrandom::fill(&mut bytes) {
        eprintln!(
            "Warning: failed to read random bytes for reviewer jitter: {}. Falling back to 2 seconds.",
            e
        );
        return 2;
    }
    2 + (bytes[0] as u64 % 4)
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

    #[test]
    fn reviewer_start_delays_are_staggered() {
        let delays = reviewer_start_delays(3);
        assert_eq!(delays.len(), 3);
        assert_eq!(delays[0], Duration::from_secs(0));
        assert!((2..=5).contains(&delays[1].as_secs()));
        assert!((4..=10).contains(&delays[2].as_secs()));
    }

    #[test]
    fn review_agent_request_uses_state_dir_with_repo_read_access() {
        let review_context = ReviewContextArtifacts {
            default_branch: "main".to_string(),
            full_diff_path: PathBuf::from("/state/20260320120000-diff-feature.patch"),
            diff_stat_path: PathBuf::from("/state/20260320120000-diffstat-feature.txt"),
            changed_files_path: PathBuf::from("/state/20260320120000-files-feature.txt"),
            changed_file_count: 2,
            diff_bytes: 1234,
        };
        let request = build_review_agent_request(
            Path::new("/repo"),
            Path::new("/state"),
            Path::new("/state/review.md"),
            &review_context,
        );

        assert_eq!(request.working_dir, PathBuf::from("/state"));
        assert!(request.allow_repo_access);
        assert!(!request.use_sandbox);
        assert!(request.prompt.contains("git -C /repo"));
        assert!(
            request
                .prompt
                .contains("/state/20260320120000-diff-feature.patch")
        );
        assert!(
            request
                .prompt
                .contains("Do NOT edit repository files, create files in the repository")
        );
    }

    #[test]
    fn build_review_context_paths_are_round_scoped() {
        let (full_diff, diff_stat, changed_files) =
            build_review_context_paths(Path::new("/state"), "feature", "20260320120000nabcdef");

        assert_eq!(
            full_diff,
            PathBuf::from("/state/20260320120000nabcdef-diff-feature.patch")
        );
        assert_eq!(
            diff_stat,
            PathBuf::from("/state/20260320120000nabcdef-diffstat-feature.txt")
        );
        assert_eq!(
            changed_files,
            PathBuf::from("/state/20260320120000nabcdef-files-feature.txt")
        );
    }

    #[test]
    fn write_review_context_artifacts_writes_expected_files() {
        let dir = tempfile::tempdir().unwrap();
        let changed_files = vec!["src/main.rs".to_string(), "src/review.rs".to_string()];

        let context = write_review_context_artifacts(
            dir.path(),
            "feature",
            "20260320120000nabcdef",
            "main",
            "diff body",
            "diff stat",
            &changed_files,
        )
        .unwrap();

        assert_eq!(context.default_branch, "main");
        assert_eq!(context.changed_file_count, 2);
        assert_eq!(context.diff_bytes, "diff body".len());
        assert_eq!(
            std::fs::read_to_string(&context.full_diff_path).unwrap(),
            "diff body"
        );
        assert_eq!(
            std::fs::read_to_string(&context.diff_stat_path).unwrap(),
            "diff stat"
        );
        assert_eq!(
            std::fs::read_to_string(&context.changed_files_path).unwrap(),
            "src/main.rs\nsrc/review.rs\n"
        );
    }
}
