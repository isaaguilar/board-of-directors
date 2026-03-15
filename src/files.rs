use crate::paths;
use std::fs;
use std::path::{Path, PathBuf};

const BUGFIX_LOG: &str = "bugfix.log.md";

pub fn ensure_state_dir(repo_root: &Path) -> Result<PathBuf, String> {
    let state_dir = paths::ensure_repo_state_dir(repo_root)?;
    migrate_legacy_state_dir(repo_root, &state_dir)?;
    Ok(state_dir)
}

/// Remove all generated review .md files from the state directory, except the bugfix log.
pub fn clean_state_dir(state_dir: &Path) -> Result<u32, String> {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(state_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".md") && *name_str != *BUGFIX_LOG {
                fs::remove_file(entry.path())
                    .map_err(|e| format!("Failed to remove {}: {}", name_str, e))?;
                count += 1;
            }
        }
    }
    Ok(count)
}

pub fn bugfix_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(BUGFIX_LOG)
}

fn migrate_legacy_state_dir(repo_root: &Path, state_dir: &Path) -> Result<(), String> {
    let legacy_dir = paths::legacy_repo_state_dir(repo_root);
    if !legacy_dir.exists() {
        return Ok(());
    }

    let mut migrated_any = false;
    let entries = fs::read_dir(&legacy_dir).map_err(|e| {
        format!(
            "Failed to read legacy state directory {}: {}",
            legacy_dir.display(),
            e
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read legacy state entry: {}", e))?;
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "Failed to read file type for {}: {}",
                entry.path().display(),
                e
            )
        })?;
        if !file_type.is_file() {
            continue;
        }

        let source = entry.path();
        let destination = state_dir.join(entry.file_name());
        if destination.exists() {
            continue;
        }

        fs::copy(&source, &destination).map_err(|e| {
            format!(
                "Failed to migrate legacy state from {} to {}: {}",
                source.display(),
                destination.display(),
                e
            )
        })?;
        fs::remove_file(&source).map_err(|e| {
            format!(
                "Migrated {} but failed to remove legacy file {}: {}",
                destination.display(),
                source.display(),
                e
            )
        })?;
        migrated_any = true;
    }

    if is_dir_empty(&legacy_dir)? {
        fs::remove_dir(&legacy_dir).map_err(|e| {
            format!(
                "Failed to remove empty legacy state directory {}: {}",
                legacy_dir.display(),
                e
            )
        })?;
    }

    if migrated_any {
        println!(
            "Migrated legacy state from {} to {}",
            legacy_dir.display(),
            state_dir.display()
        );
    }

    Ok(())
}

fn is_dir_empty(path: &Path) -> Result<bool, String> {
    let mut entries = fs::read_dir(path)
        .map_err(|e| format!("Failed to read directory {}: {}", path.display(), e))?;
    Ok(entries.next().is_none())
}
