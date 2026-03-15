use crate::agents;
use crate::config::Config;
use crate::copilot_cli;
use crate::files;
use crate::git;
use std::io::{self, BufRead, Write};
use std::path::Path;

/// Interactive consolidation -- user picks which review files to consolidate.
pub async fn run(config: &Config) -> Result<(), String> {
    let repo_root = git::repo_root()?;
    let state_dir = files::ensure_state_dir(&repo_root)?;

    let all_files = agents::list_review_files(&state_dir);
    if all_files.is_empty() {
        return Err(format!(
            "No review files found in {}. Run 'bod review' first.",
            state_dir.display()
        ));
    }

    let codenames: Vec<String> = config
        .review
        .models
        .iter()
        .map(|m| m.codename.clone())
        .collect();
    let groups = agents::group_reviews_by_round(&all_files, &codenames);
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
    io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|e| format!("Failed to read input: {}", e))?;

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

    let branch = git::current_branch()?;
    let sanitized = agents::sanitize_branch_name(&branch);
    let number = agents::next_consolidated_number(&state_dir, &sanitized);
    let out_filename = agents::consolidated_filename(&sanitized, number);
    let out_path = state_dir.join(&out_filename);

    let stdout = run_consolidation(
        &repo_root,
        &state_dir,
        &selected_files,
        &out_path,
        false,
        "",
        &config.consolidate.model,
    )
    .await?;

    // Write stdout as fallback if copilot didn't create the file via tool use
    if !out_path.exists() {
        if stdout.trim().is_empty() {
            return Err("Consolidation produced no output.".to_string());
        }
        tokio::fs::write(&out_path, stdout.as_bytes())
            .await
            .map_err(|e| format!("Failed to write consolidated report: {}", e))?;
    }

    println!("\nConsolidated report saved to: {}", out_path.display());
    Ok(())
}

/// Non-interactive consolidation -- auto-selects all review files.
/// Uses severity-tagged prompt for bugfix parsing.
/// Returns the report content as a string.
pub async fn run_auto(bod_dir: &Path, consolidate_model: &str) -> Result<String, String> {
    let all_files = agents::list_review_files(bod_dir);
    if all_files.is_empty() {
        return Err(format!("No review files found in {}.", bod_dir.display()));
    }

    let repo_root = git::repo_root()?;
    let branch = git::current_branch()?;
    let sanitized = agents::sanitize_branch_name(&branch);
    let number = agents::next_consolidated_number(bod_dir, &sanitized);
    let out_filename = agents::consolidated_filename(&sanitized, number);
    let out_path = bod_dir.join(&out_filename);

    // Read bugfix log if it exists so the consolidator knows what's been fixed
    let log_path = files::bugfix_log_path(bod_dir);
    let bugfix_log = std::fs::read_to_string(&log_path).unwrap_or_default();

    println!("  Consolidating {} review file(s)...", all_files.len());

    let stdout = run_consolidation(
        &repo_root,
        bod_dir,
        &all_files,
        &out_path,
        true,
        &bugfix_log,
        consolidate_model,
    )
    .await?;

    // Write stdout as fallback if copilot didn't create the file via tool use
    if !out_path.exists() {
        if stdout.trim().is_empty() {
            return Err("Consolidation produced no output.".to_string());
        }
        tokio::fs::write(&out_path, stdout.as_bytes())
            .await
            .map_err(|e| format!("Failed to write consolidated report: {}", e))?;
    }

    // Always read the file -- copilot may have written richer content via tool use
    // than what appeared on stdout
    let content = tokio::fs::read_to_string(&out_path)
        .await
        .map_err(|e| format!("Failed to read consolidated report: {}", e))?;

    println!("  Consolidated report: {}", out_path.display());
    Ok(content)
}

/// Shared consolidation logic. If `severity_tags` is true, the prompt asks for
/// severity tags on each finding. `bugfix_log` provides prior fix history so the
/// consolidator can mark resolved issues appropriately.
async fn run_consolidation(
    repo_root: &Path,
    bod_dir: &Path,
    files: &[String],
    out_path: &Path,
    severity_tags: bool,
    bugfix_log: &str,
    model: &str,
) -> Result<String, String> {
    let mut reviews_content = String::new();
    for filename in files {
        let path = bod_dir.join(filename);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", filename, e))?;
        reviews_content.push_str(&format!(
            "\n--- Review from {} ---\n{}\n",
            filename, content
        ));
    }

    let out_path_str = out_path.to_string_lossy().to_string();

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

Example format:
## Common Findings

[CRITICAL] Buffer overflow in parse_input -- the slice at line 42 can panic on multi-byte UTF-8

[HIGH-RESOLVED] Missing null check in user_lookup -- fixed in iteration 2 by adding guard clause
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

    let prompt = format!(
        r#"You are a senior engineering lead consolidating code review feedback from multiple independent reviewers.

You have been given reviews from different AI agents who independently reviewed the same code changes. Your task:

1. **Common Findings** (highest priority): Identify issues that multiple reviewers flagged. These are the most important since independent reviewers converged on them.
2. **Unique Findings**: List issues that only one reviewer found, but are still worth addressing.
3. **Outliers** (lowest priority): List observations that seem like edge cases or minor style preferences. Put these at the end.
4. **Final Verdict**: Provide a brief overall assessment and prioritized list of what the developer should fix first.

Format as clean, readable markdown. Be concise and actionable.
- Never run git commands. Never create commits. Never push.
- Use the supplied review files and bugfix log only as tooling inputs for synthesis.
- Do not treat their filenames, paths, or mere presence as repository defects.
- Only write the requested consolidated report file.
{severity_instruction}{bugfix_log_section}
Write the complete consolidated report to: {out_path_str}

Here are the individual reviews:

{reviews_content}"#
    );

    let output = copilot_cli::command(&prompt, model, repo_root, bod_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to start copilot for consolidation: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Consolidation copilot failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout)
}
