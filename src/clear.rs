use crate::{agents, bugfix_log, files, paths};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearMode {
    Default,
    Reviews,
    All,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ClearSummary {
    review_artifacts_removed: usize,
    bugfix_logs_cleared: usize,
    bugfix_logs_removed: usize,
}

impl ClearSummary {
    fn is_empty(&self) -> bool {
        self.review_artifacts_removed == 0
            && self.bugfix_logs_cleared == 0
            && self.bugfix_logs_removed == 0
    }
}

pub fn run(repo_root: &Path, mode: ClearMode) -> Result<(), String> {
    let state_dir = paths::repo_state_dir(repo_root);
    let summary = run_in_state_dir(&state_dir, mode)?;

    if summary.is_empty() {
        println!("Nothing to clear in {}.", state_dir.display());
        return Ok(());
    }

    match mode {
        ClearMode::Reviews => {
            println!(
                "Cleared {} review artifact(s) from {}.",
                summary.review_artifacts_removed,
                state_dir.display()
            );
        }
        ClearMode::Default => {
            println!(
                "Cleared {} review artifact(s) and reset {} bugfix log(s) while keeping saved notes.",
                summary.review_artifacts_removed,
                summary.bugfix_logs_cleared
            );
        }
        ClearMode::All => {
            println!(
                "Cleared {} review artifact(s) and removed {} bugfix log(s).",
                summary.review_artifacts_removed,
                summary.bugfix_logs_removed
            );
        }
    }

    Ok(())
}

fn run_in_state_dir(state_dir: &Path, mode: ClearMode) -> Result<ClearSummary, String> {
    if !state_dir.exists() {
        return Ok(ClearSummary::default());
    }

    let review_artifacts_removed = clear_review_artifacts(state_dir)?;
    let mut summary = ClearSummary {
        review_artifacts_removed,
        ..ClearSummary::default()
    };

    match mode {
        ClearMode::Reviews => {}
        ClearMode::Default => {
            summary.bugfix_logs_cleared = clear_bugfix_history(state_dir)?;
        }
        ClearMode::All => {
            summary.bugfix_logs_removed = clear_bugfix_logs(state_dir)?;
        }
    }

    Ok(summary)
}

fn clear_review_artifacts(state_dir: &Path) -> Result<usize, String> {
    let names: BTreeSet<String> = agents::list_timestamped_review_files(state_dir)
        .into_iter()
        .chain(agents::list_consolidated_files(state_dir))
        .chain(agents::list_review_context_artifact_files(state_dir))
        .collect();
    remove_named_files(state_dir, names)
}

fn clear_bugfix_history(state_dir: &Path) -> Result<usize, String> {
    let mut cleared = 0;
    for path in list_bugfix_log_paths(state_dir)? {
        if bugfix_log::clear_history_preserving_notes_file(&path)? {
            cleared += 1;
        }
    }
    Ok(cleared)
}

fn clear_bugfix_logs(state_dir: &Path) -> Result<usize, String> {
    let mut removed = 0;
    for path in list_bugfix_log_paths(state_dir)? {
        if remove_if_exists(&path)? {
            removed += 1;
        }
    }
    for path in list_bugfix_lock_paths(state_dir)? {
        let _ = remove_if_exists(&path)?;
    }
    Ok(removed)
}

fn remove_named_files(
    state_dir: &Path,
    names: impl IntoIterator<Item = String>,
) -> Result<usize, String> {
    let mut removed = 0;
    for name in names {
        if remove_if_exists(&state_dir.join(name))? {
            removed += 1;
        }
    }
    Ok(removed)
}

fn list_bugfix_log_paths(state_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(state_dir).map_err(|e| {
        format!(
            "Failed to read state directory {}: {}",
            state_dir.display(),
            e
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!(
                "Failed to read entry in state directory {}: {}",
                state_dir.display(),
                e
            )
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if files::is_bugfix_log(name) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn list_bugfix_lock_paths(state_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(state_dir).map_err(|e| {
        format!(
            "Failed to read state directory {}: {}",
            state_dir.display(),
            e
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!(
                "Failed to read entry in state directory {}: {}",
                state_dir.display(),
                e
            )
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(base) = name.strip_suffix(".lock") else {
            continue;
        };
        if files::is_bugfix_log(base) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn remove_if_exists(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Failed to remove {}: {}", path.display(), error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_reviews_removes_review_artifacts_only() {
        let dir = tempfile::tempdir().unwrap();
        seed_state_dir(dir.path());

        run_in_state_dir(dir.path(), ClearMode::Reviews).unwrap();

        assert!(!dir.path().join("20260320120000nabcdef-opus-main.md").exists());
        assert!(!dir.path().join("20260320120000nabcdef-consolidated-main.md").exists());
        assert!(!dir.path().join("consolidated-main.md").exists());
        assert!(!dir.path().join("20260320120000nabcdef-diff-main.patch").exists());
        assert!(!dir.path().join("20260320120000nabcdef-diffstat-main.txt").exists());
        assert!(!dir.path().join("20260320120000nabcdef-files-main.txt").exists());
        assert!(dir.path().join("bugfix-main.log.md").exists());
        assert!(dir.path().join("config.toml").exists());
        assert!(dir.path().join("keep-me.md").exists());
    }

    #[test]
    fn clear_default_preserves_notes_and_removes_bugfix_history() {
        let dir = tempfile::tempdir().unwrap();
        seed_state_dir(dir.path());

        run_in_state_dir(dir.path(), ClearMode::Default).unwrap();

        let main_parts = bugfix_log::read_log_parts_with_migration(dir.path(), "main").unwrap();
        assert_eq!(main_parts.notes, "remember this");
        assert_eq!(main_parts.history, "");
        assert!(dir.path().join("bugfix-main.log.md").exists());
        assert!(!dir.path().join("bugfix-feature.log.md").exists());
        assert!(dir.path().join("config.toml").exists());
        assert!(dir.path().join("keep-me.md").exists());
    }

    #[test]
    fn clear_all_removes_bugfix_logs_and_locks() {
        let dir = tempfile::tempdir().unwrap();
        seed_state_dir(dir.path());
        std::fs::write(dir.path().join("bugfix-main.log.md.lock"), "").unwrap();
        std::fs::write(dir.path().join("bugfix-orphan.log.md.lock"), "").unwrap();

        run_in_state_dir(dir.path(), ClearMode::All).unwrap();

        assert!(!dir.path().join("bugfix-main.log.md").exists());
        assert!(!dir.path().join("bugfix-feature.log.md").exists());
        assert!(!dir.path().join("bugfix-main.log.md.lock").exists());
        assert!(!dir.path().join("bugfix-orphan.log.md.lock").exists());
        assert!(dir.path().join("config.toml").exists());
        assert!(dir.path().join("keep-me.md").exists());
    }

    fn seed_state_dir(state_dir: &Path) {
        std::fs::write(state_dir.join("config.toml"), "config = true").unwrap();
        std::fs::write(
            state_dir.join("20260320120000nabcdef-opus-main.md"),
            "review",
        )
        .unwrap();
        std::fs::write(
            state_dir.join("20260320120000nabcdef-consolidated-main.md"),
            "consolidated",
        )
        .unwrap();
        std::fs::write(state_dir.join("consolidated-main.md"), "legacy consolidated").unwrap();
        std::fs::write(
            state_dir.join("20260320120000nabcdef-diff-main.patch"),
            "patch",
        )
        .unwrap();
        std::fs::write(
            state_dir.join("20260320120000nabcdef-diffstat-main.txt"),
            "diffstat",
        )
        .unwrap();
        std::fs::write(
            state_dir.join("20260320120000nabcdef-files-main.txt"),
            "files",
        )
        .unwrap();
        std::fs::write(state_dir.join("keep-me.md"), "notes").unwrap();

        bugfix_log::write_user_notes(state_dir, "main", "remember this").unwrap();
        bugfix_log::write_history_preserving_notes(
            state_dir,
            "main",
            "## Iteration 1\nkeep this out of the cleared log\n",
        )
        .unwrap();
        std::fs::write(
            state_dir.join("bugfix-feature.log.md"),
            "## Iteration 2\nhistory only\n",
        )
        .unwrap();
    }
}
