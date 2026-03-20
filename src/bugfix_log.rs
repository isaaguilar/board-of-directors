use crate::files;
use fs2::FileExt;
use std::io::Write;
use std::path::Path;

const USER_NOTES_START: &str = "<!-- BOD_USER_NOTES_START -->";
const USER_NOTES_END: &str = "<!-- BOD_USER_NOTES_END -->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugfixLogParts {
    pub notes: String,
    pub history: String,
    pub full: String,
}

pub fn ensure_user_notes_section(state_dir: &Path, sanitized_branch: &str) -> Result<(), String> {
    let path = files::bugfix_log_path(state_dir, sanitized_branch)?;
    let lock_path = path.with_extension("md.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open lock file {}: {}", lock_path.display(), e))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| format!("Failed to acquire lock on {}: {}", lock_path.display(), e))?;
    let result = (|| {
        let existing = files::read_bugfix_log_with_migration(state_dir, sanitized_branch)?;
        // If a notes section already exists, leave it intact to preserve user notes.
        if note_bounds(&existing).is_some() {
            return Ok(());
        }
        let updated = with_notes_section(&existing, "");
        if updated != existing {
            std::fs::write(&path, updated)
                .map_err(|e| format!("Failed to write bugfix log {}: {}", path.display(), e))?;
        }
        Ok(())
    })();
    let _ = lock_file.unlock();
    result
}

pub fn read_user_notes_with_migration(
    state_dir: &Path,
    sanitized_branch: &str,
) -> Result<String, String> {
    Ok(read_log_parts_with_migration(state_dir, sanitized_branch)?.notes)
}

pub fn read_log_parts_with_migration(
    state_dir: &Path,
    sanitized_branch: &str,
) -> Result<BugfixLogParts, String> {
    let full = files::read_bugfix_log_with_migration(state_dir, sanitized_branch)?;
    Ok(split_log(&full))
}

pub fn write_user_notes(
    state_dir: &Path,
    sanitized_branch: &str,
    notes: &str,
) -> Result<String, String> {
    let path = files::bugfix_log_path(state_dir, sanitized_branch)?;
    // Use an exclusive file lock to serialize concurrent writes from the
    // web server (dashboard) and the fix agent subprocess.
    let lock_path = path.with_extension("md.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open lock file {}: {}", lock_path.display(), e))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| format!("Failed to acquire lock on {}: {}", lock_path.display(), e))?;
    let result = (|| {
        let existing = files::read_bugfix_log_with_migration(state_dir, sanitized_branch)?;
        let updated = replace_notes(&existing, notes);
        let mut file = std::fs::File::create(&path)
            .map_err(|e| format!("Failed to create bugfix log {}: {}", path.display(), e))?;
        file.write_all(updated.as_bytes())
            .map_err(|e| format!("Failed to write bugfix log {}: {}", path.display(), e))?;
        Ok(updated)
    })();
    let _ = lock_file.unlock();
    result
}

pub fn append_user_notes(
    state_dir: &Path,
    sanitized_branch: &str,
    extra: &str,
) -> Result<String, String> {
    let existing = read_user_notes_with_migration(state_dir, sanitized_branch)?;
    let combined = if existing.trim().is_empty() {
        extra.to_string()
    } else {
        format!("{}\n{}", existing, extra)
    };
    write_user_notes(state_dir, sanitized_branch, &combined)
}

pub fn write_history_preserving_notes(
    state_dir: &Path,
    sanitized_branch: &str,
    history: &str,
) -> Result<String, String> {
    let path = files::bugfix_log_path(state_dir, sanitized_branch)?;
    let lock_path = path.with_extension("md.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open lock file {}: {}", lock_path.display(), e))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| format!("Failed to acquire lock on {}: {}", lock_path.display(), e))?;
    let result = (|| {
        let existing = files::read_bugfix_log_with_migration(state_dir, sanitized_branch)?;
        let current_notes = split_log(&existing).notes;
        let updated = with_notes_section(history, &current_notes);
        let mut file = std::fs::File::create(&path)
            .map_err(|e| format!("Failed to create bugfix log {}: {}", path.display(), e))?;
        file.write_all(updated.as_bytes())
            .map_err(|e| format!("Failed to write bugfix log {}: {}", path.display(), e))?;
        Ok(updated)
    })();
    let _ = lock_file.unlock();
    result
}

pub fn clear_history_preserving_notes_file(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }

    let lock_path = path.with_extension("md.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open lock file {}: {}", lock_path.display(), e))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| format!("Failed to acquire lock on {}: {}", lock_path.display(), e))?;

    enum ClearOutcome {
        Unchanged,
        Rewritten,
        Removed,
    }

    let result = (|| -> Result<ClearOutcome, String> {
        let existing = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read bugfix log {}: {}", path.display(), e))?;
        let parts = split_log(&existing);
        if parts.notes.trim().is_empty() {
            std::fs::remove_file(path)
                .map_err(|e| format!("Failed to remove bugfix log {}: {}", path.display(), e))?;
            return Ok(ClearOutcome::Removed);
        }

        let updated = render_notes_section(&parts.notes);
        if updated == existing {
            return Ok(ClearOutcome::Unchanged);
        }

        std::fs::write(path, updated)
            .map_err(|e| format!("Failed to write bugfix log {}: {}", path.display(), e))?;
        Ok(ClearOutcome::Rewritten)
    })();

    let _ = lock_file.unlock();
    let removed_log = matches!(result, Ok(ClearOutcome::Removed));
    drop(lock_file);
    if removed_log {
        let _ = std::fs::remove_file(&lock_path);
    }

    match result? {
        ClearOutcome::Unchanged => Ok(false),
        ClearOutcome::Rewritten | ClearOutcome::Removed => Ok(true),
    }
}

fn split_log(content: &str) -> BugfixLogParts {
    if let Some((start, end)) = note_bounds(content) {
        let notes_start = start + USER_NOTES_START.len();
        let notes = content[notes_start..end].trim_matches('\n').to_string();
        let history = strip_leading_separator(&content[end + USER_NOTES_END.len()..]).to_string();
        return BugfixLogParts {
            notes,
            history,
            full: content.to_string(),
        };
    }

    BugfixLogParts {
        notes: String::new(),
        history: content.to_string(),
        full: content.to_string(),
    }
}

fn note_bounds(content: &str) -> Option<(usize, usize)> {
    let start = content.find(USER_NOTES_START)?;
    let end = content.find(USER_NOTES_END)?;
    if end < start {
        None
    } else {
        Some((start, end))
    }
}

fn render_notes_section(notes: &str) -> String {
    let trimmed = notes.trim_end();
    if trimmed.is_empty() {
        format!(
            "## User Notes\n\n{}\n\n{}\n",
            USER_NOTES_START, USER_NOTES_END
        )
    } else {
        format!(
            "## User Notes\n\n{}\n{}\n{}\n",
            USER_NOTES_START, trimmed, USER_NOTES_END
        )
    }
}

fn with_notes_section(content: &str, notes: &str) -> String {
    if note_bounds(content).is_some() {
        replace_notes(content, notes)
    } else if content.trim().is_empty() {
        render_notes_section(notes)
    } else {
        format!(
            "{}\n\n---\n\n{}",
            render_notes_section(notes).trim_end(),
            content.trim_start()
        )
    }
}

fn replace_notes(content: &str, notes: &str) -> String {
    if let Some((start, end)) = note_bounds(content) {
        let mut updated = String::new();
        let notes_start = start + USER_NOTES_START.len();
        updated.push_str(&content[..notes_start]);
        updated.push('\n');
        let trimmed = notes.trim_end();
        if !trimmed.is_empty() {
            updated.push_str(trimmed);
            updated.push('\n');
        } else {
            updated.push('\n');
        }
        updated.push_str(&content[end..]);
        updated
    } else {
        with_notes_section(content, notes)
    }
}

fn strip_leading_separator(content: &str) -> &str {
    let trimmed = content.trim_start_matches(['\r', '\n']);
    if let Some(rest) = trimmed.strip_prefix("---") {
        rest.trim_start_matches(['\r', '\n'])
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_user_notes_section_creates_log_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        ensure_user_notes_section(dir.path(), "main").unwrap();

        let path = files::bugfix_log_path(dir.path(), "main").unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains(USER_NOTES_START));
        assert!(content.contains(USER_NOTES_END));
    }

    #[test]
    fn read_log_parts_treats_legacy_content_as_history() {
        let parts = split_log("## Iteration 1\nlegacy");
        assert_eq!(parts.notes, "");
        assert_eq!(parts.history, "## Iteration 1\nlegacy");
    }

    #[test]
    fn write_user_notes_prepends_notes_section_without_losing_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = files::bugfix_log_path(dir.path(), "main").unwrap();
        std::fs::write(&path, "## Iteration 1\nhistory").unwrap();

        write_user_notes(dir.path(), "main", "remember this").unwrap();
        let parts = read_log_parts_with_migration(dir.path(), "main").unwrap();

        assert_eq!(parts.notes, "remember this");
        assert_eq!(parts.history, "## Iteration 1\nhistory");
        assert!(parts.full.contains("## User Notes"));
    }

    #[test]
    fn write_user_notes_updates_existing_notes_section() {
        let dir = tempfile::tempdir().unwrap();
        ensure_user_notes_section(dir.path(), "main").unwrap();
        write_user_notes(dir.path(), "main", "first note").unwrap();
        write_user_notes(dir.path(), "main", "second note").unwrap();

        let parts = read_log_parts_with_migration(dir.path(), "main").unwrap();
        assert_eq!(parts.notes, "second note");
        assert!(!parts.full.contains("first note"));
    }

    #[test]
    fn ensure_user_notes_section_preserves_existing_notes() {
        let dir = tempfile::tempdir().unwrap();
        ensure_user_notes_section(dir.path(), "main").unwrap();
        write_user_notes(dir.path(), "main", "keep this note").unwrap();

        // Calling ensure again must NOT erase the existing notes.
        ensure_user_notes_section(dir.path(), "main").unwrap();

        let parts = read_log_parts_with_migration(dir.path(), "main").unwrap();
        assert_eq!(parts.notes, "keep this note");
    }

    #[test]
    fn write_history_preserving_notes_restores_history_without_overwriting_current_notes() {
        let dir = tempfile::tempdir().unwrap();
        ensure_user_notes_section(dir.path(), "main").unwrap();
        let path = files::bugfix_log_path(dir.path(), "main").unwrap();
        std::fs::write(
            &path,
            "## User Notes\n\n<!-- BOD_USER_NOTES_START -->\nold note\n<!-- BOD_USER_NOTES_END -->\n\n---\n\n## Iteration 1\nkept history\n",
        )
        .unwrap();

        write_user_notes(dir.path(), "main", "new note").unwrap();
        write_history_preserving_notes(dir.path(), "main", "## Iteration 1\nkept history\n")
            .unwrap();

        let parts = read_log_parts_with_migration(dir.path(), "main").unwrap();
        assert_eq!(parts.notes, "new note");
        assert_eq!(parts.history, "## Iteration 1\nkept history\n");
    }

    #[test]
    fn clear_history_preserving_notes_file_keeps_notes_only() {
        let dir = tempfile::tempdir().unwrap();
        write_user_notes(dir.path(), "main", "keep this").unwrap();
        write_history_preserving_notes(dir.path(), "main", "## Iteration 1\nhistory\n").unwrap();

        let path = files::bugfix_log_path(dir.path(), "main").unwrap();
        assert!(clear_history_preserving_notes_file(&path).unwrap());

        let parts = read_log_parts_with_migration(dir.path(), "main").unwrap();
        assert_eq!(parts.notes, "keep this");
        assert_eq!(parts.history, "");
    }

    #[test]
    fn clear_history_preserving_notes_file_removes_logs_without_notes() {
        let dir = tempfile::tempdir().unwrap();
        let path = files::bugfix_log_path(dir.path(), "main").unwrap();
        std::fs::write(&path, "## Iteration 1\nhistory only\n").unwrap();

        assert!(clear_history_preserving_notes_file(&path).unwrap());
        assert!(!path.exists());
        assert!(!path.with_extension("md.lock").exists());
    }
}
