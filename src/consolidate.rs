use crate::agents;
use crate::backend;
use crate::bugfix_log;
use crate::config::{Backend, Config};
use crate::files;
use crate::git;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsolidationAgentRequest {
    prompt: String,
    working_dir: PathBuf,
    allow_repo_access: bool,
    use_sandbox: bool,
}

/// Interactive consolidation -- user picks which review files to consolidate.
pub async fn run(config: &Config) -> Result<(), String> {
    let repo_root = git::repo_root()?;
    let state_dir = files::ensure_state_dir(&repo_root)?;
    let branch = git::current_branch()?;
    let sanitized = agents::sanitize_branch_name(&branch).ok_or_else(|| {
        format!(
            "Branch name '{}' contains no alphanumeric characters and cannot be used for filenames.",
            branch
        )
    })?;

    let all_files = agents::list_review_files(&state_dir);
    if all_files.is_empty() {
        return Err(format!(
            "No review files found in {}. Run 'bod review' first.",
            state_dir.display()
        ));
    }

    let groups = agents::group_reviews_by_round(&all_files);
    let mut round_keys: Vec<String> = groups.keys().cloned().collect();
    round_keys.sort();

    println!("Available review sets:\n");
    for (i, key) in round_keys.iter().enumerate() {
        let files = &groups[key];
        println!("  [{}] {} ({} file(s))", i + 1, key, files.len());
        for f in files {
            println!("      - {}", f);
        }
    }
    println!("\n  [a] All files\n");

    print!("Select review set(s) to consolidate (comma-separated numbers, or 'a' for all): ");
    io::stdout()
        .flush()
        .map_err(|e| format!("IO error: {}", e))?;

    let mut input = String::new();
    let bytes = io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|e| format!("Failed to read input: {}", e))?;
    if bytes == 0 {
        return Err("Unexpected end of input".to_string());
    }

    let input = input.trim();
    let selected_files: Vec<String> = if input == "a" || input == "A" {
        all_files.clone()
    } else {
        let mut selected = Vec::new();
        for part in input.split(',') {
            let part = part.trim();
            if let Ok(idx) = part.parse::<usize>() {
                if idx >= 1 && idx <= round_keys.len() {
                    let key = &round_keys[idx - 1];
                    selected.extend(groups[key].clone());
                } else {
                    return Err(format!("Invalid selection: {}", idx));
                }
            } else {
                return Err(format!("Invalid input: '{}'", part));
            }
        }
        selected
    };

    if selected_files.is_empty() {
        return Err("No files selected.".to_string());
    }

    println!("\nConsolidating {} review file(s)...", selected_files.len());

    write_selected_reviews(
        &config.consolidate.backend,
        &repo_root,
        &state_dir,
        &selected_files,
        &config.consolidate.model,
        &sanitized,
    )
    .await
}

/// Non-interactive consolidation of the latest review run for the current branch.
pub async fn run_latest(config: &Config) -> Result<(), String> {
    let repo_root = git::repo_root()?;
    let state_dir = files::ensure_state_dir(&repo_root)?;
    let all_files = agents::list_review_files(&state_dir);
    if all_files.is_empty() {
        return Err(format!(
            "No review files found in {}. Run 'bod review' first.",
            state_dir.display()
        ));
    }

    let branch = git::current_branch()?;
    let sanitized = agents::sanitize_branch_name(&branch).ok_or_else(|| {
        format!(
            "Branch name '{}' contains no alphanumeric characters and cannot be used for filenames.",
            branch
        )
    })?;
    let codenames: Vec<String> = config
        .review
        .models
        .iter()
        .map(|m| m.codename.clone())
        .collect();

    let selected_files = agents::latest_review_files(&all_files, &codenames, &sanitized)
        .ok_or_else(|| {
            format!(
                "No review files found for the latest '{}' review run in {}. Run 'bod review' first.",
                branch,
                state_dir.display()
            )
        })?;

    println!(
        "\nConsolidating latest '{}' review run ({} file(s))...",
        branch,
        selected_files.len()
    );

    write_selected_reviews(
        &config.consolidate.backend,
        &repo_root,
        &state_dir,
        &selected_files,
        &config.consolidate.model,
        &sanitized,
    )
    .await
}

async fn write_selected_reviews(
    backend: &Backend,
    repo_root: &Path,
    state_dir: &Path,
    selected_files: &[String],
    model: &str,
    sanitized_branch: &str,
) -> Result<(), String> {
    let timestamp = agents::timestamp_now();
    let (out_filename, mut guard) =
        agents::create_consolidated_file(&state_dir, sanitized_branch, &timestamp)
            .map_err(|e| format!("Failed to reserve consolidated file: {}", e))?;
    let out_path = state_dir.join(&out_filename);

    let stdout = run_consolidation(
        backend,
        &repo_root,
        &state_dir,
        &selected_files,
        &out_path,
        false,
        "",
        "",
        model,
    )
    .await?;

    // Check file size, not just existence, in case the agent created a 0-byte file.
    let meta = tokio::fs::metadata(&out_path).await.ok();
    let file_is_empty = meta.map_or(true, |m| m.len() == 0);
    if file_is_empty {
        let clean = backend::strip_ansi_codes(&stdout);
        if clean.trim().is_empty() {
            return Err("Consolidation produced no output.".to_string());
        }
        tokio::fs::write(&out_path, clean.as_bytes())
            .await
            .map_err(|e| format!("Failed to write consolidated report: {}", e))?;
    }

    // File has valid content -- disarm before optional ANSI read-back.
    guard.disarm();

    // Strip ANSI codes from agent-written content (mirrors run_auto).
    let raw = tokio::fs::read_to_string(&out_path)
        .await
        .map_err(|e| format!("Failed to read consolidated report: {}", e))?;
    let content = backend::strip_ansi_codes(&raw);
    if content != raw {
        if let Err(e) = tokio::fs::write(&out_path, content.as_bytes()).await {
            eprintln!("Warning: failed to rewrite ANSI-cleaned report: {}", e);
        }
    }

    println!("\nConsolidated report saved to: {}", out_path.display());
    Ok(())
}

/// Non-interactive consolidation -- scoped to a specific review round.
/// Uses severity-tagged prompt for bugfix parsing.
/// When `review_timestamp` is provided, only review files from that round are included.
/// Returns the report content as a string.
pub async fn run_auto(
    backend: &Backend,
    bod_dir: &Path,
    consolidate_model: &str,
    review_timestamp: Option<&str>,
    codenames: &[String],
    repo_root: &Path,
    sanitized_branch: &str,
) -> Result<String, String> {
    let all_files = match review_timestamp {
        Some(ts) => {
            agents::list_review_files_for_round_id(bod_dir, ts, Some(sanitized_branch), codenames)
        }
        None => agents::list_review_files(bod_dir),
    };
    if all_files.is_empty() {
        return Err(format!("No review files found in {}.", bod_dir.display()));
    }

    let timestamp = review_timestamp
        .map(|ts| ts.to_string())
        .unwrap_or_else(agents::timestamp_now);
    let (out_filename, mut guard) =
        agents::create_consolidated_file(bod_dir, sanitized_branch, &timestamp)
            .map_err(|e| format!("Failed to reserve consolidated file: {}", e))?;
    let out_path = bod_dir.join(&out_filename);

    // Read bugfix log for the current branch, migrating from the legacy
    // global log if the branch-scoped file does not yet exist.
    let bugfix_log = bugfix_log::read_log_parts_with_migration(bod_dir, sanitized_branch)?;

    println!("  Consolidating {} review file(s)...", all_files.len());

    let stdout = run_consolidation(
        backend,
        repo_root,
        bod_dir,
        &all_files,
        &out_path,
        true,
        &bugfix_log.history,
        &bugfix_log.notes,
        consolidate_model,
    )
    .await?;

    // Check file size, not just existence, in case the agent created a 0-byte file.
    let meta = tokio::fs::metadata(&out_path).await.ok();
    let file_is_empty = meta.map_or(true, |m| m.len() == 0);
    if file_is_empty {
        let clean = backend::strip_ansi_codes(&stdout);
        if clean.trim().is_empty() {
            return Err("Consolidation produced no output.".to_string());
        }
        tokio::fs::write(&out_path, clean.as_bytes())
            .await
            .map_err(|e| format!("Failed to write consolidated report: {}", e))?;
    }

    // File has valid content at this point -- disarm before read-back.
    guard.disarm();

    // Always read the file -- the agent may have written richer content via tool use
    // than what appeared on stdout. Strip ANSI codes in case the agent wrote them.
    let raw = tokio::fs::read_to_string(&out_path)
        .await
        .map_err(|e| format!("Failed to read consolidated report: {}", e))?;
    let content = backend::strip_ansi_codes(&raw);

    // Write the cleaned content back to disk if ANSI stripping changed it,
    // so the on-disk file is always clean markdown.
    if content != raw {
        tokio::fs::write(&out_path, content.as_bytes())
            .await
            .map_err(|e| format!("Failed to re-write cleaned consolidated report: {}", e))?;
    }

    println!("  Consolidated report: {}", out_path.display());
    Ok(content)
}

/// Shared consolidation logic. If `severity_tags` is true, the prompt asks for
/// severity tags on each finding. `bugfix_log` provides prior fix history so the
/// consolidator can mark resolved issues appropriately.
async fn run_consolidation(
    backend: &Backend,
    repo_root: &Path,
    bod_dir: &Path,
    files: &[String],
    out_path: &Path,
    severity_tags: bool,
    bugfix_log: &str,
    user_notes: &str,
    model: &str,
) -> Result<String, String> {
    let mut reviews_content = String::new();
    for filename in files {
        let path = bod_dir.join(filename);
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", filename, e))?;
        let content = backend::strip_ansi_codes(&raw);
        reviews_content.push_str(&format!(
            "\n--- Review from {} ---\n{}\n",
            filename, content
        ));
    }

    let request = build_consolidation_agent_request(
        repo_root,
        bod_dir,
        out_path,
        severity_tags,
        bugfix_log,
        user_notes,
        &reviews_content,
    );

    let output = backend::run_agent(
        backend,
        &request.prompt,
        model,
        &request.working_dir,
        request.allow_repo_access,
        request.use_sandbox,
        repo_root,
        bod_dir,
    )
    .await
    .map_err(|e| {
        if backend::is_arg_too_long(&e) {
            "Prompt exceeds OS argument-size limit (E2BIG). \
                 The diff may be too large for command-line passing. \
                 Consider reviewing a smaller changeset."
                .to_string()
        } else {
            format!("Failed to start agent for consolidation: {}", e)
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Consolidation agent failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout)
}

fn build_consolidation_agent_request(
    repo_root: &Path,
    bod_dir: &Path,
    out_path: &Path,
    severity_tags: bool,
    bugfix_log: &str,
    user_notes: &str,
    reviews_content: &str,
) -> ConsolidationAgentRequest {
    let out_path_str = out_path.to_string_lossy().to_string();
    let repo_root_str = repo_root.to_string_lossy().to_string();

    let severity_instruction = if severity_tags {
        r#"
IMPORTANT: Prefix EVERY finding with a severity tag on its own line.

For issues that STILL NEED ACTION (not yet fixed):
- `[CRITICAL]` for bugs that will cause crashes, data loss, or security vulnerabilities
- `[HIGH]` for logic errors, incorrect behavior, or significant correctness issues
- `[MEDIUM]` for code quality issues, missing error handling, or minor correctness concerns
- `[LOW]` for style, naming, or minor suggestions

For issues that have ALREADY BEEN FIXED (confirmed in the bugfix log or evident in the code):
- `[CRITICAL-RESOLVED]` was critical but is now fixed
- `[HIGH-RESOLVED]` was high but is now fixed
- `[MEDIUM-RESOLVED]` was medium but is now fixed
- `[LOW-RESOLVED]` was low but is now fixed

You MUST use the -RESOLVED suffix for any issue that the bugfix log confirms has been addressed.
Do NOT use bare [CRITICAL] or [HIGH] for issues that are already fixed -- that causes
the automated fixer to re-attempt fixes that are already done, wasting time and risking regressions.

CRITICAL RULE -- each severity tag must appear EXACTLY ONCE per unique issue, in the finding
body only. Do NOT repeat severity tags in the Final Verdict or anywhere else in the report.
The Final Verdict section must reference findings by title or description only -- no `[CRITICAL]`,
`[HIGH]`, `[MEDIUM]`, or `[LOW]` tags. These tags are parsed by deterministic code that counts
every occurrence; a tag repeated in a summary will be treated as a separate additional issue
and trigger a duplicate fix attempt.

Example format:
## Common Findings

[CRITICAL] Buffer overflow in parse_input -- the slice at line 42 can panic on multi-byte UTF-8

[HIGH-RESOLVED] Missing null check in user_lookup -- fixed in iteration 2 by adding guard clause

## Final Verdict

Fix the buffer overflow in parse_input before merging. (No severity tag here -- reference by title only.)
"#
    } else {
        ""
    };

    let bugfix_log_section = if !bugfix_log.is_empty() {
        format!(
            r#"

IMPORTANT CONTEXT -- Prior fixes already applied:
The following bugfix log documents changes that have already been made to the codebase.
Cross-reference this log when assigning severity tags. If an issue described by a reviewer
has already been fixed according to this log, use the -RESOLVED suffix on its severity tag.

--- Bugfix Log ---
{}
--- End Bugfix Log ---
"#,
            bugfix_log
        )
    } else {
        String::new()
    };

    let user_notes_section = if !user_notes.trim().is_empty() {
        format!(
            r#"

IMPORTANT CONTEXT -- Operator notes for the NEXT iteration:
The following notes were written by the human while `bod bugfix` was running.
Treat them as additional context for prioritization and investigation, but do not
invent issues that are unsupported by the reviews or code.

--- User Notes ---
{}
--- End User Notes ---
"#,
            user_notes
        )
    } else {
        String::new()
    };

    let prompt = format!(
        r#"You are a senior engineering lead consolidating code review feedback from multiple independent reviewers.

You have been given reviews from different AI agents who independently reviewed the same code changes. Your task:

1. **Common Findings** (highest priority): Identify issues that multiple reviewers flagged. These are the most important since independent reviewers converged on them. If reviewers found no common issues, explicitly say so.
2. **Unique Findings**: List issues that only one reviewer found, but are still worth addressing. If there are none, say so.
3. **Outliers** (lowest priority): List observations that seem like edge cases or minor style preferences. Put these at the end. If there are none, omit this section.
4. **Final Verdict**: Provide a brief overall assessment and prioritized list of what the developer should fix first. If the code is fundamentally correct and no meaningful issues were found, say that clearly.

Do not manufacture findings to fill sections. An empty finding list is a valid and correct outcome. Only report issues that the reviewers actually raised and that genuinely exist in the code.

Format as clean, readable markdown. Be concise and actionable.
- Do NOT run `git commit` or `git push`.
- You may inspect repository files and use read-only git commands for research when helpful. Because your current working directory is tooling state outside the repository, run git commands as `git -C {repo_root_str} <args>` so they target the repository explicitly.
- Do NOT edit repository files, create files in the repository, or use write tools against repository paths.
- Your current working directory is tooling state outside the repository.
- Use the supplied review files and bugfix log only as tooling inputs for synthesis.
- Do not treat their filenames, paths, or mere presence as repository defects.
- Only write the requested consolidated report file.
  {severity_instruction}{bugfix_log_section}{user_notes_section}
Write the complete consolidated report to: {out_path_str}

Here are the individual reviews:

{reviews_content}"#
    );

    ConsolidationAgentRequest {
        prompt,
        working_dir: bod_dir.to_path_buf(),
        allow_repo_access: true,
        use_sandbox: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consolidation_agent_request_uses_state_dir_with_repo_read_access() {
        let request = build_consolidation_agent_request(
            Path::new("/repo"),
            Path::new("/state"),
            Path::new("/state/consolidated.md"),
            true,
            "",
            "",
            "--- Review from a ---\n[HIGH] issue",
        );

        assert_eq!(request.working_dir, PathBuf::from("/state"));
        assert!(request.allow_repo_access);
        assert!(!request.use_sandbox);
        assert!(request.prompt.contains("git -C /repo"));
        assert!(
            request
                .prompt
                .contains("Only write the requested consolidated report file.")
        );
    }
}
