use crate::agents;
use crate::backend;
use crate::config::{Backend, Config};
use crate::consolidate;
use crate::files;
use crate::git;
use crate::review;
use regex::Regex;
use std::fmt;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

    /// Returns all severity levels at or above this threshold.
    fn included_levels(&self) -> Vec<&'static str> {
        match self {
            SeverityLevel::Critical => vec!["CRITICAL"],
            SeverityLevel::High => vec!["CRITICAL", "HIGH"],
            SeverityLevel::Medium => vec!["CRITICAL", "HIGH", "MEDIUM"],
            SeverityLevel::Low => vec!["CRITICAL", "HIGH", "MEDIUM", "LOW"],
        }
    }
}

pub async fn run(
    timeout_secs: u64,
    severity: SeverityLevel,
    config: &Config,
) -> Result<(), String> {
    let repo_root = git::repo_root()?;
    let state_dir = files::ensure_state_dir(&repo_root)?;
    let branch = git::current_branch()?;
    let sanitized_branch = agents::sanitize_branch_name(&branch).ok_or_else(|| {
        format!(
            "Branch name '{}' contains no alphanumeric characters and cannot be used for filenames.",
            branch
        )
    })?;

    let log_path = files::bugfix_log_path(&state_dir, &sanitized_branch)
        .map_err(|e| format!("Invalid branch for bugfix log path: {}", e))?;
    let included = severity.included_levels();

    println!(
        "Bugfix mode: fixing {} and above (threshold: {})",
        severity,
        included.join(", ")
    );

    let start = Instant::now();
    let mut iteration = 0;

    loop {
        iteration += 1;
        let elapsed = start.elapsed().as_secs();

        if elapsed >= timeout_secs {
            println!(
                "\n== Timeout reached ({} seconds). Stopping after {} iteration(s). ==",
                timeout_secs,
                iteration - 1
            );
            break;
        }

        let remaining = timeout_secs - elapsed;
        println!(
            "\n============================================================\n== Bugfix iteration {} ({}s remaining) ==\n============================================================",
            iteration, remaining
        );

        // Periodic cleanup: keep only the last 10 rounds of review files
        if let Err(e) = agents::cleanup_old_rounds(&state_dir, 10) {
            eprintln!("Warning: failed to clean up old review files: {}", e);
        }

        // Step 1: Run multi-agent review
        println!("\n-- Step 1: Running multi-agent review --");
        let review_timestamp = match review::run(config).await {
            Ok(ts) => ts,
            Err(e) => {
                eprintln!("  Review step failed: {}", e);
                if e.is_fatal() {
                    eprintln!("  Fatal review setup error. Stopping.");
                    return Err(e.to_string());
                }
                match e.timestamp {
                    Some(ts) => {
                        eprintln!("  Partial review success -- consolidating available reviews.");
                        ts
                    }
                    None => {
                        eprintln!("  Continuing to next iteration...");
                        continue;
                    }
                }
            }
        };

        if start.elapsed().as_secs() >= timeout_secs {
            println!("\n== Timeout reached after review step. Stopping. ==");
            break;
        }

        // Step 2: Auto-consolidate (scoped to the current review round)
        println!("\n-- Step 2: Consolidating reviews --");
        let codenames: Vec<String> = config
            .review
            .models
            .iter()
            .map(|m| m.codename.clone())
            .collect();
        let report = match consolidate::run_auto(&config.backend, &state_dir, &config.consolidate.model, Some(&review_timestamp), &codenames, &repo_root, &sanitized_branch).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  Consolidation failed: {}", e);
                eprintln!("  Continuing to next iteration...");
                continue;
            }
        };

        // Step 3: Count issues at or above the severity threshold
        let counts = count_severities(&report, &included);
        let total_actionable: u32 = counts.iter().map(|(_, c)| c).sum();

        let summary: Vec<String> = counts.iter().map(|(l, c)| format!("{} {}", c, l)).collect();
        println!("\n  Severity summary: {}", summary.join(", "));

        if total_actionable == 0 {
            println!(
                "\n== No issues at {} or above found. Code looks good! ==",
                severity
            );
            break;
        }

        if start.elapsed().as_secs() >= timeout_secs {
            println!("\n== Timeout reached before fix step. Stopping. ==");
            println!("  {} issue(s) remain.", total_actionable);
            break;
        }

        let prior_log = files::read_bugfix_log_with_migration(&state_dir, &sanitized_branch)?;

        println!(
            "\n-- Step 3: Fixing {} issue(s) with {} --",
            total_actionable, config.bugfix.model
        );
        if let Err(e) = run_fix_agent(
            &config.backend,
            &repo_root,
            &state_dir,
            &report,
            &prior_log,
            &log_path,
            iteration,
            &severity,
            &config.bugfix.model,
        )
        .await
        {
            eprintln!("  Fix step failed: {}", e);
            eprintln!("  Continuing to next iteration...");
            continue;
        }

        println!("\n  Fix step complete. Starting next review cycle...");
    }

    let total_elapsed = start.elapsed().as_secs();
    println!(
        "\n== Bugfix finished after {} iteration(s) in {}s ==",
        iteration, total_elapsed
    );

    Ok(())
}

/// Count unresolved issues for each severity level in the included set.
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

/// Extract findings at or above the severity threshold.
fn extract_actionable(report: &str, severity: &SeverityLevel) -> String {
    let included = severity.included_levels();
    let included_tags: Vec<String> = included.iter().map(|l| format!("[{}]", l)).collect();

    // Tags that mark a finding as below the threshold (not actionable)
    let all_levels = ["CRITICAL", "HIGH", "MEDIUM", "LOW"];
    let excluded_tags: Vec<String> = all_levels
        .iter()
        .filter(|l| !included.contains(l))
        .map(|l| format!("[{}]", l))
        .collect();

    // Resolved tags are never actionable
    let resolved_tags: Vec<String> = all_levels
        .iter()
        .map(|l| format!("[{}-RESOLVED]", l))
        .collect();

    let mut findings = Vec::new();
    let mut current_finding: Option<String> = None;
    let mut is_relevant = false;

    let flush = |finding: Option<String>, relevant: bool, out: &mut Vec<String>| {
        if let Some(f) = finding
            && relevant
        {
            out.push(f);
        }
    };

    for line in report.lines() {
        let upper = line.to_uppercase();

        let is_included = included_tags.iter().any(|t| upper.contains(t));
        let is_excluded = excluded_tags.iter().any(|t| upper.contains(t));
        let is_resolved = resolved_tags.iter().any(|t| upper.contains(t));

        if is_resolved {
            flush(current_finding.take(), is_relevant, &mut findings);
            current_finding = Some(line.to_string());
            is_relevant = false;
        } else if is_included {
            flush(current_finding.take(), is_relevant, &mut findings);
            current_finding = Some(line.to_string());
            is_relevant = true;
        } else if is_excluded {
            flush(current_finding.take(), is_relevant, &mut findings);
            current_finding = Some(line.to_string());
            is_relevant = false;
        } else if line.starts_with("## ") || line.starts_with("# ") {
            flush(current_finding.take(), is_relevant, &mut findings);
            is_relevant = false;
        } else if let Some(ref mut f) = current_finding {
            f.push('\n');
            f.push_str(line);
        }
    }

    flush(current_finding, is_relevant, &mut findings);

    if findings.is_empty() {
        report.to_string()
    } else {
        findings.join("\n\n")
    }
}

async fn run_fix_agent(
    backend: &Backend,
    repo_root: &Path,
    state_dir: &Path,
    report: &str,
    prior_log: &str,
    log_path: &Path,
    iteration: u32,
    severity: &SeverityLevel,
    fix_model: &str,
) -> Result<(), String> {
    let issues = extract_actionable(report, severity);
    let log_path_str = log_path.to_string_lossy().to_string();
    let levels_label = severity.included_levels().join(", ");

    let history_section = if prior_log.is_empty() {
        "No prior fixes have been made yet. This is the first iteration.".to_string()
    } else {
        format!(
            r#"IMPORTANT: Below is the log of ALL prior fixes made in previous iterations.
Review this carefully to avoid undoing previous fixes or creating cycles where
fix A breaks B, then fixing B breaks A again.

--- Prior Fix Log ---
{}
--- End Prior Fix Log ---"#,
            prior_log
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
        - Read-only git commands for research are allowed when helpful (for example `git status`, `git diff`, `git log`, and `git show`).
        - Avoid any git command that changes the checked-out branch, commit history, index, or working tree unless it is strictly temporary research and you restore the branch to exactly the same uncommitted state and history it had before.
        - Treat files under `~/.config/board-of-directors/` as internal board-of-directors tooling state.
        - Do NOT inspect or use that tooling state as repository source material.
- The only allowed interaction with tooling state is appending the required summary to the fix log below.
- Do NOT create new documentation files or write documentation from scratch unless editing an existing doc is directly required to complete a correctness fix.
- Do NOT create new test files unless a fix specifically requires one.

{history_section}

Here are the issues to fix:

{issues}

AFTER you have made all fixes, append a summary to the fix log file at: {log_path_str}

The summary MUST use this exact format (append, do not overwrite):

## Iteration {iteration}

For each fix, write:
### [issue title or short description]
- **What changed**: [files and lines modified]
- **Why**: [what was wrong and why this fix is correct]
- **Risk**: [any risk of regression or interaction with other fixes]

---
"#
    );

    let output = backend::run_agent(backend, &prompt, fix_model, repo_root, state_dir)
        .await
        .map_err(|e| {
            if backend::is_arg_too_long(&e) {
                "Prompt exceeds OS argument-size limit (E2BIG). \
                 The diff may be too large for command-line passing. \
                 Consider reviewing a smaller changeset.".to_string()
            } else {
                format!("Failed to start fix agent: {}", e)
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Fix agent failed: {}", stderr));
    }

    println!("  Fix agent completed.");
    Ok(())
}
