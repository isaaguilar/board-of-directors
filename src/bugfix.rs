use crate::agents;
use crate::backend;
use crate::bugfix_log;
use crate::bugfix_session::{BugfixSession, SessionStatus};
use crate::config::{Backend, Config};
use crate::consolidate;
use crate::files;
use crate::git;
use crate::review;
use crate::rollback;
use crate::web;
use regex::Regex;
use serde::Serialize;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SeverityLevel {
    Critical,
    High,
    Medium,
    Low,
}

impl fmt::Display for SeverityLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SeverityLevel::Critical => write!(f, "critical"),
            SeverityLevel::High => write!(f, "high"),
            SeverityLevel::Medium => write!(f, "medium"),
            SeverityLevel::Low => write!(f, "low"),
        }
    }
}

impl SeverityLevel {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "critical" => Ok(SeverityLevel::Critical),
            "high" => Ok(SeverityLevel::High),
            "medium" => Ok(SeverityLevel::Medium),
            "low" => Ok(SeverityLevel::Low),
            _ => Err(format!(
                "Invalid severity '{}'. Must be: critical, high, medium, low",
                s
            )),
        }
    }

    fn included_levels(&self) -> Vec<&'static str> {
        match self {
            SeverityLevel::Critical => vec!["CRITICAL"],
            SeverityLevel::High => vec!["CRITICAL", "HIGH"],
            SeverityLevel::Medium => vec!["CRITICAL", "HIGH", "MEDIUM"],
            SeverityLevel::Low => vec!["CRITICAL", "HIGH", "MEDIUM", "LOW"],
        }
    }
}

enum StepOutcome<T> {
    Completed(T),
    Cancelled,
}

enum FixAgentOutcome {
    Completed(Result<(), String>),
    Cancelled,
}

enum ManualStartOutcome {
    Started,
    Cancelled,
}

const REVIEW_FILE_RETENTION: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FixAgentRequest {
    prompt: String,
    working_dir: PathBuf,
    allow_repo_access: bool,
    use_sandbox: bool,
}

pub async fn run(
    timeout_secs: u64,
    max_iterations: Option<u32>,
    severity: SeverityLevel,
    config: &Config,
    cli_prompt: Option<&str>,
    delay_start: bool,
    no_open: bool,
    dry_run: bool,
) -> Result<(), String> {
    let repo_root = git::repo_root()?;
    let state_dir = files::ensure_state_dir(&repo_root)?;
    let repo_name = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let branch = git::current_branch()?;
    let sanitized_branch = agents::sanitize_branch_name(&branch).ok_or_else(|| {
        format!(
            "Branch name '{}' contains no alphanumeric characters and cannot be used for filenames.",
            branch
        )
    })?;

    let log_path = files::bugfix_log_path(&state_dir, &sanitized_branch)
        .map_err(|e| format!("Invalid branch for bugfix log path: {}", e))?;

    if dry_run {
        println!(
            "{}",
            render_dry_run_summary(
                config,
                &repo_root,
                &state_dir,
                &branch,
                &sanitized_branch,
                &log_path,
                timeout_secs,
                max_iterations,
                severity,
                cli_prompt,
                delay_start,
                no_open,
            )
        );
        return Ok(());
    }

    bugfix_log::ensure_user_notes_section(&state_dir, &sanitized_branch)?;

    if let Some(prompt) = cli_prompt {
        bugfix_log::append_user_notes(&state_dir, &sanitized_branch, prompt)?;
    }

    let session = BugfixSession::new(
        state_dir.clone(),
        repo_name,
        branch.clone(),
        sanitized_branch.clone(),
        config
            .review
            .models
            .iter()
            .map(|model| model.codename.clone())
            .collect(),
        timeout_secs,
        severity,
        log_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "bugfix.log.md".to_string()),
    );

    let mut server = web::start(session.clone()).await?;
    println!(
        "Bugfix mode: fixing {} and above (threshold: {})",
        severity,
        severity.included_levels().join(", ")
    );
    println!("Dashboard: {} (port {})", server.url, server.port);
    if no_open {
        println!("Automatic browser launch is disabled. Open the URL manually if you want the dashboard.");
    } else {
        match web::open_browser(&server.url) {
            Ok(()) => println!("Opened the live dashboard in your browser."),
            Err(e) => eprintln!("Warning: {} Open {} manually.", e, server.url),
        }
    }

    let started = if delay_start {
        session.mark_waiting_to_start().await;
        spawn_terminal_start_listener(session.clone());
        println!(
            "Delayed start is enabled. Click Start in the dashboard or press Enter here to begin."
        );
        match wait_for_manual_start(&session).await {
            ManualStartOutcome::Started => {
                println!("\n== Bugfix session starting. ==");
                true
            }
            ManualStartOutcome::Cancelled => {
                session
                    .mark_cancelled("Cancelled before the bugfix session started.")
                    .await;
                println!("\n== Bugfix cancelled before start. ==");
                false
            }
        }
    } else {
        true
    };

    let start = if started {
        session.mark_run_started().await;
        Some(Instant::now())
    } else {
        None
    };
    let mut iteration = 0u32;

    while started {
        if session.is_cancel_requested().await {
            session
                .mark_cancelled(
                    "Cancelled before a new fix step began. Any already-completed fix steps were kept.",
                )
                .await;
            println!("\n== Bugfix cancelled from the browser UI. ==");
            break;
        }

        let elapsed = start
            .as_ref()
            .expect("start instant present once bugfix starts")
            .elapsed()
            .as_secs();
        if elapsed >= timeout_secs {
            session
                .mark_timed_out(format!("Timeout reached after {} iteration(s).", iteration))
                .await;
            println!(
                "\n== Timeout reached ({} seconds). Stopping after {} iteration(s). ==",
                timeout_secs, iteration
            );
            break;
        }

        if let Some(max) = max_iterations {
            if iteration >= max {
                session
                    .mark_completed(format!(
                        "Iteration limit reached after {} iteration(s).",
                        iteration
                    ))
                    .await;
                println!("\n== Iteration limit ({}) reached. Stopping. ==", max);
                break;
            }
        }

        iteration += 1;
        session.set_will_revert_on_cancel(false).await;

        let remaining = timeout_secs - elapsed;
        let active_severity = session
            .activate_iteration(
                iteration,
                format!("Bugfix iteration {} ({}s remaining)", iteration, remaining),
            )
            .await;
        let included = active_severity.included_levels();

        println!(
            "\n============================================================\n== Bugfix iteration {} ({}s remaining, threshold: {}) ==\n============================================================",
            iteration, remaining, active_severity
        );

        if let Err(e) = agents::cleanup_old_rounds(&state_dir, REVIEW_FILE_RETENTION) {
            eprintln!("Warning: failed to clean up old review files: {}", e);
        }

        println!("\n-- Step 1: Running multi-agent review --");
        let review_timestamp = match review::run_for_bugfix(config, &session).await {
            Ok(ts) => ts,
            Err(e) => {
                eprintln!("  Review step failed: {}", e);
                if session.is_cancel_requested().await {
                    session
                        .mark_cancelled("Cancelled while review agents were running.")
                        .await;
                    println!("\n== Bugfix cancelled during review. ==");
                    break;
                }
                if e.is_fatal() {
                    session.mark_error(e.to_string()).await;
                    eprintln!("  Fatal review setup error. Stopping.");
                    return Err(e.to_string());
                }
                match e.timestamp {
                    Some(ts) => {
                        eprintln!("  Partial review success -- consolidating available reviews.");
                        ts
                    }
                    None => {
                        let message = terminal_step_failure("Review step failed", e.to_string());
                        session.mark_error(message.clone()).await;
                        return Err(message);
                    }
                }
            }
        };

        if session.is_cancel_requested().await {
            session
                .mark_cancelled("Cancelled after the review step. No code changes were applied.")
                .await;
            println!("\n== Bugfix cancelled after review. ==");
            break;
        }

        if start
            .as_ref()
            .expect("start instant present once bugfix starts")
            .elapsed()
            .as_secs()
            >= timeout_secs
        {
            session
                .mark_timed_out("Timeout reached after the review step.")
                .await;
            println!("\n== Timeout reached after review step. Stopping. ==");
            break;
        }

        let consolidate_label = format!(
            "{} / {}",
            config.consolidate.backend, config.consolidate.model
        );
        println!(
            "\n-- Step 2: Consolidating reviews with {} --",
            consolidate_label
        );
        session.begin_consolidation(&consolidate_label).await;
        let codenames: Vec<String> = config
            .review
            .models
            .iter()
            .map(|model| model.codename.clone())
            .collect();

        let report = match run_cancellable(
            &session,
            consolidate::run_auto(
                &config.consolidate.backend,
                &state_dir,
                &config.consolidate.model,
                Some(&review_timestamp),
                &codenames,
                &repo_root,
                &sanitized_branch,
            ),
        )
        .await
        {
            StepOutcome::Completed(Ok(report)) => {
                let report_filename = find_consolidated_report_filename(
                    &state_dir,
                    &sanitized_branch,
                    &review_timestamp,
                );
                session.set_latest_report(report_filename).await;
                session.complete_consolidation(&consolidate_label).await;
                report
            }
            StepOutcome::Completed(Err(e)) => {
                eprintln!("  Consolidation failed: {}", e);
                session
                    .fail_consolidation(&consolidate_label, e.to_string())
                    .await;
                let message = terminal_step_failure("Consolidation failed", e);
                session.mark_error(message.clone()).await;
                return Err(message);
            }
            StepOutcome::Cancelled => {
                session
                    .mark_cancelled(
                        "Cancelled while the consolidator was running. No code changes were applied.",
                    )
                    .await;
                println!("\n== Bugfix cancelled during consolidation. ==");
                break;
            }
        };

        let counts = count_severities(&report, &included);
        let total_actionable: u32 = counts.iter().map(|(_, count)| *count).sum();
        let summary: Vec<String> = counts
            .iter()
            .map(|(level, count)| format!("{} {}", count, level))
            .collect();
        session
            .set_severity_counts(counts.clone(), total_actionable)
            .await;

        println!("\n  Severity summary: {}", summary.join(", "));

        if total_actionable == 0 {
            session
                .mark_completed(format!(
                    "No issues at {} or above were found. The code looks good.",
                    active_severity
                ))
                .await;
            println!(
                "\n== No issues at {} or above found. Code looks good! ==",
                active_severity
            );
            break;
        }

        if start
            .as_ref()
            .expect("start instant present once bugfix starts")
            .elapsed()
            .as_secs()
            >= timeout_secs
        {
            session
                .mark_timed_out(format!(
                    "Timeout reached before the fix step. {} issue(s) remain.",
                    total_actionable
                ))
                .await;
            println!("\n== Timeout reached before fix step. Stopping. ==");
            println!("  {} issue(s) remain.", total_actionable);
            break;
        }

        if session.is_cancel_requested().await {
            session
                .mark_cancelled(
                    "Cancelled before the fix step began. No code changes were applied.",
                )
                .await;
            println!("\n== Bugfix cancelled before the fix step. ==");
            break;
        }

        let prior_log = bugfix_log::read_log_parts_with_migration(&state_dir, &sanitized_branch)?;
        let snapshot = rollback::capture(&repo_root)?;
        session.set_will_revert_on_cancel(true).await;
        let bugfix_label = format!("{} / {}", config.bugfix.backend, config.bugfix.model);
        session.begin_fix(total_actionable, &bugfix_label).await;

        println!(
            "\n-- Step 3: Fixing {} issue(s) with {} --",
            total_actionable, bugfix_label
        );

        match run_fix_agent(
            &session,
            &config.bugfix.backend,
            &repo_root,
            &state_dir,
            &report,
            &prior_log.history,
            &prior_log.notes,
            &log_path,
            iteration,
            &review_timestamp,
            &active_severity,
            &config.bugfix.model,
        )
        .await
        {
            FixAgentOutcome::Completed(Ok(())) => {
                session.set_will_revert_on_cancel(false).await;
                session.complete_fix(&bugfix_label).await;
                session
                    .set_message("Fix step complete. Starting the next review cycle...")
                    .await;
                println!("\n  Fix step complete. Starting next review cycle...");
            }
            FixAgentOutcome::Completed(Err(e)) => {
                session.set_will_revert_on_cancel(false).await;
                eprintln!("  Fix step failed: {}", e);
                if let Err(re) = restore_fix_step_state(
                    &repo_root,
                    &snapshot,
                    &state_dir,
                    &sanitized_branch,
                    &prior_log.history,
                ) {
                    let message = format!(
                        "Fix step failed and restoring the pre-fix-step state also failed: {}",
                        re
                    );
                    session.mark_error(message.clone()).await;
                    return Err(message);
                }
                session.fail_fix(&bugfix_label, e.to_string()).await;
                let message = terminal_step_failure("Fix step failed", e);
                session.mark_error(message.clone()).await;
                return Err(message);
            }
            FixAgentOutcome::Cancelled => {
                if let Err(e) = restore_fix_step_state(
                    &repo_root,
                    &snapshot,
                    &state_dir,
                    &sanitized_branch,
                    &prior_log.history,
                ) {
                    let message = format!(
                        "Cancel requested during the fix step, but restoring the pre-fix-step state failed: {}",
                        e
                    );
                    session.mark_error(message.clone()).await;
                    return Err(message);
                }
                session
                    .mark_cancelled(
                        "Cancelled during the fix step. Restored the repo to the snapshot taken before this fix step started. Earlier iteration changes and any pre-existing branch changes were kept.",
                    )
                    .await;
                println!("\n== Bugfix cancelled during the fix step. Changes reverted. ==");
                break;
            }
        }
    }

    let total_elapsed = start
        .map(|started_at| started_at.elapsed().as_secs())
        .unwrap_or(0);
    if started {
        println!(
            "\n== Bugfix finished after {} iteration(s) in {}s ==",
            iteration, total_elapsed
        );
    } else {
        println!("\n== Bugfix session ended before the first iteration started. ==");
    }
    println!("Dashboard is still available at {}", server.url);
    println!("Close from the browser or press Ctrl+C to exit.");

    tokio::select! {
        _ = server.wait_for_quit() => {
            println!("\n== Session closed from the browser. ==");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n== Interrupted. Shutting down. ==");
        }
    }

    let final_snapshot = session.snapshot().await;
    server.shutdown();

    final_result_from_status(
        final_snapshot.status,
        final_snapshot.last_error.as_deref(),
        &final_snapshot.latest_message,
    )
}

fn final_result_from_status(
    status: SessionStatus,
    last_error: Option<&str>,
    latest_message: &str,
) -> Result<(), String> {
    match status {
        SessionStatus::Error => Err(last_error
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Bugfix finished with an error state.".to_string())),
        SessionStatus::TimedOut => Err(last_error
            .map(|s| s.to_string())
            .unwrap_or_else(|| latest_message.to_string())),
        _ => Ok(()),
    }
}

fn terminal_step_failure(prefix: &str, detail: impl Into<String>) -> String {
    format!("{prefix}: {}", detail.into())
}

fn render_dry_run_summary(
    config: &Config,
    repo_root: &Path,
    state_dir: &Path,
    branch: &str,
    sanitized_branch: &str,
    log_path: &Path,
    timeout_secs: u64,
    max_iterations: Option<u32>,
    severity: SeverityLevel,
    cli_prompt: Option<&str>,
    delay_start: bool,
    no_open: bool,
) -> String {
    let reviewers = config
        .review
        .models
        .iter()
        .map(|entry| format!("- {}: {} / {}", entry.codename, entry.backend, entry.model))
        .collect::<Vec<_>>()
        .join("\n");
    let iteration_limit = max_iterations
        .map(|max| max.to_string())
        .unwrap_or_else(|| "unbounded (stops when clean or timed out)".to_string());
    let prompt_note = if cli_prompt.is_some() {
        "A CLI prompt was supplied, but dry-run mode does not append notes to the bugfix log."
    } else {
        "No extra CLI prompt was supplied."
    };

    format!(
        r#"Bugfix dry run only. No agents, dashboard, browser launch, bugfix-log writes, or repo changes will occur.

Repository root: {}
State dir: {}
Branch: {}
Sanitized branch: {}
Bugfix log path: {}
Timeout: {}s
Iteration limit: {}
Severity threshold: {}
Manual start: {}
Browser auto-open: {}
Review artifact cleanup limit: {}
Reviewers:
{}
Consolidator: {} / {}
Fixer: {} / {}
Fixer execution:
- working directory: {}
- repository access: source edits are limited to {}
- bugfix log appends must go directly to {}
- repo-local temporary .md/.txt files are forbidden
{}"#,
        repo_root.display(),
        state_dir.display(),
        branch,
        sanitized_branch,
        log_path.display(),
        timeout_secs,
        iteration_limit,
        severity,
        if delay_start { "enabled" } else { "disabled" },
        if no_open { "disabled" } else { "enabled" },
        REVIEW_FILE_RETENTION,
        reviewers,
        config.consolidate.backend,
        config.consolidate.model,
        config.bugfix.backend,
        config.bugfix.model,
        state_dir.display(),
        repo_root.display(),
        log_path.display(),
        prompt_note
    )
}

fn count_severities(report: &str, included: &[&str]) -> Vec<(String, u32)> {
    included
        .iter()
        .map(|level| {
            let pattern = format!(r"(?i)\[{}\]", regex::escape(level));
            let re = Regex::new(&pattern).unwrap();
            let count = re.find_iter(report).count() as u32;
            (level.to_string(), count)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_step_failure_matches_operator_message() {
        assert_eq!(
            terminal_step_failure("Review step failed", "3 agent(s) failed."),
            "Review step failed: 3 agent(s) failed."
        );
    }

    #[test]
    fn final_result_returns_error_status_message() {
        assert_eq!(
            final_result_from_status(SessionStatus::Error, Some("boom"), "ignored"),
            Err("boom".to_string())
        );
    }

    #[test]
    fn final_result_returns_timeout_message_when_no_error_is_present() {
        assert_eq!(
            final_result_from_status(SessionStatus::TimedOut, None, "Timeout reached after review step."),
            Err("Timeout reached after review step.".to_string())
        );
    }

    #[test]
    fn final_result_accepts_completed_status() {
        assert_eq!(
            final_result_from_status(SessionStatus::Completed, None, "done"),
            Ok(())
        );
    }

    #[test]
    fn dry_run_summary_skips_agents_and_shows_fixer_context() {
        let config = Config {
            review: crate::config::ReviewConfig {
                models: vec![crate::config::ModelEntry {
                    codename: "reviewer-one".to_string(),
                    backend: Backend::Copilot,
                    model: "gpt-5-mini".to_string(),
                }],
            },
            consolidate: crate::config::ConsolidateConfig {
                backend: Backend::ClaudeCode,
                model: "sonnet".to_string(),
            },
            bugfix: crate::config::BugfixConfig {
                backend: Backend::GeminiCli,
                model: "flash".to_string(),
            },
        };

        let summary = render_dry_run_summary(
            &config,
            Path::new("/repo"),
            Path::new("/state"),
            "feature",
            "feature",
            Path::new("/state/bugfix-feature.log.md"),
            3600,
            Some(2),
            SeverityLevel::High,
            Some("investigate"),
            true,
            true,
        );

        assert!(summary.contains("Bugfix dry run only. No agents"));
        assert!(summary.contains("Browser auto-open: disabled"));
        assert!(summary.contains("working directory: /state"));
        assert!(summary.contains("bugfix log appends must go directly to /state/bugfix-feature.log.md"));
        assert!(summary.contains("dry-run mode does not append notes to the bugfix log"));
    }

    #[test]
    fn fix_agent_request_uses_state_dir_and_forbids_repo_temp_files() {
        let request = build_fix_agent_request(
            Path::new("/repo"),
            Path::new("/state"),
            "[HIGH] incorrect behavior",
            "",
            "",
            Path::new("/state/bugfix-feature.log.md"),
            1,
            "20260320120000n123456789abc",
            &SeverityLevel::High,
        );

        assert_eq!(request.working_dir, PathBuf::from("/state"));
        assert!(request.allow_repo_access);
        assert!(!request.use_sandbox);
        assert!(request.prompt.contains("git -C /repo"));
        assert!(request
            .prompt
            .contains("Do NOT create temporary `.md` or `.txt` files in the repository."));
        assert!(request.prompt.contains("/state/bugfix-feature.log.md"));
    }
}

fn extract_actionable(report: &str, severity: &SeverityLevel) -> String {
    let included = severity.included_levels();
    let included_tags: Vec<String> = included
        .iter()
        .map(|level| format!("[{}]", level))
        .collect();

    let all_levels = ["CRITICAL", "HIGH", "MEDIUM", "LOW"];
    let excluded_tags: Vec<String> = all_levels
        .iter()
        .filter(|level| !included.contains(level))
        .map(|level| format!("[{}]", level))
        .collect();
    let resolved_tags: Vec<String> = all_levels
        .iter()
        .map(|level| format!("[{}-RESOLVED]", level))
        .collect();

    let mut findings = Vec::new();
    let mut current_finding: Option<String> = None;
    let mut relevant = false;

    let flush = |finding: Option<String>, relevant: bool, out: &mut Vec<String>| {
        if let Some(finding) = finding
            && relevant
        {
            out.push(finding);
        }
    };

    for line in report.lines() {
        let upper = line.to_uppercase();
        let is_included = included_tags.iter().any(|tag| upper.contains(tag));
        let is_excluded = excluded_tags.iter().any(|tag| upper.contains(tag));
        let is_resolved = resolved_tags.iter().any(|tag| upper.contains(tag));

        if is_resolved {
            flush(current_finding.take(), relevant, &mut findings);
            current_finding = Some(line.to_string());
            relevant = false;
        } else if is_included {
            flush(current_finding.take(), relevant, &mut findings);
            current_finding = Some(line.to_string());
            relevant = true;
        } else if is_excluded {
            flush(current_finding.take(), relevant, &mut findings);
            current_finding = Some(line.to_string());
            relevant = false;
        } else if line.starts_with("# ") || line.starts_with("## ") {
            flush(current_finding.take(), relevant, &mut findings);
            relevant = false;
        } else if let Some(existing) = current_finding.as_mut() {
            existing.push('\n');
            existing.push_str(line);
        }
    }

    flush(current_finding, relevant, &mut findings);
    if findings.is_empty() {
        report.to_string()
    } else {
        findings.join("\n\n")
    }
}

fn build_fix_agent_request(
    repo_root: &Path,
    state_dir: &Path,
    report: &str,
    prior_history: &str,
    user_notes: &str,
    log_path: &Path,
    iteration: u32,
    review_timestamp: &str,
    severity: &SeverityLevel,
) -> FixAgentRequest {
    let issues = extract_actionable(report, severity);
    let log_path_str = log_path.to_string_lossy().to_string();
    let repo_root_str = repo_root.to_string_lossy().to_string();
    let levels_label = severity.included_levels().join(", ");

    let history_section = if prior_history.is_empty() {
        "No prior fixes have been made yet. This is the first iteration.".to_string()
    } else {
        format!(
            r#"IMPORTANT: Below is the log of ALL prior fixes made in previous iterations.
Review this carefully to avoid undoing previous fixes or creating cycles where
fix A breaks B, then fixing B breaks A again.

--- Prior Fix Log ---
{}
--- End Prior Fix Log ---"#,
            prior_history
        )
    };

    let notes_section = if user_notes.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"

IMPORTANT: The human running `bod bugfix` wrote these notes while the loop was in progress.
Treat them as extra debugging context for the NEXT iteration and factor them into your fixes
when they line up with the code and review report.

--- User Notes ---
{}
--- End User Notes ---"#,
            user_notes
        )
    };

    let prompt = format!(
        r#"You are a senior software engineer. You have been given a code review report containing {levels_label} severity issues found in the current codebase.

Your task:
- Fix ALL of the issues listed below.
- Make precise, surgical changes. Do not refactor unrelated code.
- Prioritize correctness. Every fix must be correct.
- After making changes, verify they compile (run the project's build/check command).
- Do NOT run `git commit` or `git push`.
- Your current working directory is board-of-directors tooling state outside the repository. The repository root is `{repo_root_str}`.
- Read-only git commands for research are allowed when helpful, but run them as `git -C {repo_root_str} <args>` so they target the repository explicitly.
- When you run build, check, or test commands, target the repository explicitly (for example `cd {repo_root_str} && cargo build`).
- Avoid any git command that changes the checked-out branch, commit history, index, or working tree unless it is strictly temporary research and you restore the branch to exactly the same uncommitted state and history it had before.
- Treat files under `~/.config/board-of-directors/` as internal board-of-directors tooling state.
- Do NOT inspect or use that tooling state as repository source material.
- The only allowed interaction with tooling state is appending the required summary to the fix log below.
- Do NOT create temporary `.md` or `.txt` files in the repository. Append directly to `{log_path_str}`. If you need scratch space, use the tooling state directory or the system temp directory, not the repo checkout, and clean it up before finishing.
- Do NOT create new documentation files or write documentation from scratch unless editing an existing doc is directly required to complete a correctness fix.
- Do NOT create new test files unless a fix specifically requires one.

{history_section}{notes_section}

Here are the issues to fix:

{issues}

AFTER you have made all fixes, append a summary to the fix log file at: {log_path_str}

The summary MUST use this exact format (append, do not overwrite):

## Iteration {iteration} (round {review_timestamp})

For each fix, write:
### [issue title or short description]
- **What changed**: [files and lines modified]
- **Why**: [what was wrong and why this fix is correct]
- **Risk**: [any risk of regression or interaction with other fixes]

---
"#
    );

    FixAgentRequest {
        prompt,
        working_dir: state_dir.to_path_buf(),
        allow_repo_access: true,
        use_sandbox: false,
    }
}

async fn run_fix_agent(
    session: &BugfixSession,
    backend: &Backend,
    repo_root: &Path,
    state_dir: &Path,
    report: &str,
    prior_history: &str,
    user_notes: &str,
    log_path: &Path,
    iteration: u32,
    review_timestamp: &str,
    severity: &SeverityLevel,
    fix_model: &str,
) -> FixAgentOutcome {
    let request = build_fix_agent_request(
        repo_root,
        state_dir,
        report,
        prior_history,
        user_notes,
        log_path,
        iteration,
        review_timestamp,
        severity,
    );

    let mut cancel_rx = session.subscribe_cancel();
    let output = match backend::run_agent_cancellable(
        backend,
        &request.prompt,
        fix_model,
        &request.working_dir,
        request.allow_repo_access,
        request.use_sandbox,
        repo_root,
        state_dir,
        &mut cancel_rx,
    )
    .await
    {
        Ok(backend::AgentRunResult::Completed(output)) => output,
        Ok(backend::AgentRunResult::Cancelled) => return FixAgentOutcome::Cancelled,
        Err(e) => {
            return FixAgentOutcome::Completed(Err(if backend::is_arg_too_long(&e) {
                "Prompt exceeds OS argument-size limit (E2BIG). \
                 The diff may be too large for command-line passing. \
                 Consider reviewing a smaller changeset."
                    .to_string()
            } else {
                format!("Failed to start fix agent: {}", e)
            }));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return FixAgentOutcome::Completed(Err(format!("Fix agent failed: {}", stderr)));
    }

    println!("  Fix agent completed.");
    FixAgentOutcome::Completed(Ok(()))
}

fn restore_fix_step_state(
    repo_root: &Path,
    snapshot: &rollback::IterationSnapshot,
    state_dir: &Path,
    sanitized_branch: &str,
    prior_history: &str,
) -> Result<(), String> {
    rollback::restore(repo_root, snapshot)?;
    bugfix_log::write_history_preserving_notes(state_dir, sanitized_branch, prior_history)?;
    Ok(())
}

async fn run_cancellable<T, F>(session: &BugfixSession, future: F) -> StepOutcome<T>
where
    F: Future<Output = T>,
{
    let mut cancel_rx = session.subscribe_cancel();
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return StepOutcome::Completed(result),
            changed = cancel_rx.changed() => {
                match changed {
                    Ok(()) if *cancel_rx.borrow() => return StepOutcome::Cancelled,
                    // Sender dropped -- no more cancel signals possible.
                    // Just await the inner future to completion.
                    Err(_) => return StepOutcome::Completed(future.await),
                    _ => {}
                }
            }
        }
    }
}

fn spawn_terminal_start_listener(session: BugfixSession) {
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            handle.spawn(async move {
                if session.request_start().await {
                    println!("\n== Manual start requested from the terminal. ==");
                }
            });
        }
    });
}

async fn wait_for_manual_start(session: &BugfixSession) -> ManualStartOutcome {
    let mut start_rx = session.subscribe_start();
    let mut cancel_rx = session.subscribe_cancel();

    if *start_rx.borrow_and_update() {
        return ManualStartOutcome::Started;
    }
    if *cancel_rx.borrow_and_update() {
        return ManualStartOutcome::Cancelled;
    }

    loop {
        tokio::select! {
            changed = start_rx.changed() => {
                match changed {
                    Ok(()) if *start_rx.borrow_and_update() => return ManualStartOutcome::Started,
                    Err(_) => return ManualStartOutcome::Cancelled,
                    _ => {}
                }
            }
            changed = cancel_rx.changed() => {
                match changed {
                    Ok(()) if *cancel_rx.borrow_and_update() => return ManualStartOutcome::Cancelled,
                    Err(_) => return ManualStartOutcome::Cancelled,
                    _ => {}
                }
            }
        }
    }
}

fn find_consolidated_report_filename(
    state_dir: &Path,
    sanitized_branch: &str,
    review_timestamp: &str,
) -> Option<String> {
    let prefix = format!("{}-consolidated-{}", review_timestamp, sanitized_branch);
    let mut matches = Vec::new();

    if let Ok(entries) = std::fs::read_dir(state_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            if stem == prefix
                || stem
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_prefix('~'))
                    .map_or(false, |digits| {
                        !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                    })
            {
                matches.push(name);
            }
        }
    }

    matches.sort();
    matches.pop()
}
