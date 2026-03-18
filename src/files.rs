use crate::paths;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn ensure_state_dir(repo_root: &Path) -> Result<PathBuf, String> {
    paths::ensure_repo_state_dir(repo_root)
}

/// Returns the path for the branch-scoped bugfix log.
///
/// # Errors
///
/// Returns `Err` if `sanitized_branch` is empty or contains characters outside
/// `[a-zA-Z0-9_-]`, since either would produce a malformed or potentially
/// path-traversing filename.
pub fn bugfix_log_path(state_dir: &Path, sanitized_branch: &str) -> Result<PathBuf, String> {
    if sanitized_branch.is_empty() {
        return Err("sanitized_branch must not be empty".to_string());
    }
    if !sanitized_branch
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "sanitized_branch contains invalid characters: {:?}",
            sanitized_branch
        ));
    }
    Ok(state_dir.join(format!("bugfix-{}.log.md", sanitized_branch)))
}

pub fn legacy_bugfix_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("bugfix.log.md")
}

/// Reads the bugfix log for the given branch, performing a one-time migration
/// from the legacy `bugfix.log.md` if the branch-scoped log does not yet exist.
///
/// - If the branch-scoped log file exists (even if empty), its content is returned.
/// - If it does not exist but the legacy log has content, the legacy content is
///   copied into the branch-scoped log (one-time migration) and returned.
/// - If neither file has usable content, an empty string is returned.
///
/// I/O errors other than `NotFound` are propagated.
pub fn read_bugfix_log_with_migration(
    state_dir: &Path,
    sanitized_branch: &str,
) -> Result<String, String> {
    let branch_log_path = bugfix_log_path(state_dir, sanitized_branch)?;

    match std::fs::read_to_string(&branch_log_path) {
        Ok(content) => return Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "Failed to read bugfix log {}: {}",
                branch_log_path.display(),
                e
            ));
        }
    }

    // Branch-scoped log does not exist yet -- check for legacy log to migrate.
    let legacy_path = legacy_bugfix_log_path(state_dir);
    let legacy_content = match std::fs::read_to_string(&legacy_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => {
            return Err(format!(
                "Failed to read legacy bugfix log {}: {}",
                legacy_path.display(),
                e
            ));
        }
    };

    if legacy_content.is_empty() {
        return Ok(String::new());
    }

    eprintln!(
        "Warning: migrating legacy bugfix.log.md into branch-scoped log at {}",
        branch_log_path.display()
    );

    // Use create_new(true) to place the branch log without clobbering.
    // If a concurrent process (migration or regular bugfix run) already
    // created the branch log between our NotFound check and this open,
    // we get AlreadyExists and read the existing (possibly newer) content
    // instead of overwriting it.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&branch_log_path)
    {
        Ok(mut f) => {
            f.write_all(legacy_content.as_bytes()).map_err(|e| {
                format!(
                    "Failed to write branch log {}: {}",
                    branch_log_path.display(),
                    e
                )
            })?;
            f.flush().map_err(|e| {
                format!(
                    "Failed to flush branch log {}: {}",
                    branch_log_path.display(),
                    e
                )
            })?;
            f.sync_data().map_err(|e| {
                format!(
                    "Failed to sync branch log {}: {}",
                    branch_log_path.display(),
                    e
                )
            })?;
            Ok(legacy_content)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another process created the branch log first -- read and
            // return whatever it wrote (which may be newer than legacy).
            std::fs::read_to_string(&branch_log_path).map_err(|e| {
                format!(
                    "Failed to read branch log {}: {}",
                    branch_log_path.display(),
                    e
                )
            })
        }
        Err(e) => Err(format!(
            "Failed to create branch log {}: {}",
            branch_log_path.display(),
            e
        )),
    }
}

pub fn is_bugfix_log(name: &str) -> bool {
    if name == "bugfix.log.md" {
        return true;
    }
    if let Some(rest) = name.strip_prefix("bugfix-") {
        if let Some(branch) = rest.strip_suffix(".log.md") {
            return !branch.is_empty()
                && branch
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_bugfix_log_matches_legacy() {
        assert!(is_bugfix_log("bugfix.log.md"));
    }

    #[test]
    fn is_bugfix_log_matches_branch_scoped() {
        assert!(is_bugfix_log("bugfix-main.log.md"));
        assert!(is_bugfix_log("bugfix-feature-branch.log.md"));
    }

    #[test]
    fn is_bugfix_log_rejects_empty_branch() {
        // bugfix-.log.md corresponds to an empty branch name, which
        // bugfix_log_path rejects via Err. The filter must also reject it.
        assert!(!is_bugfix_log("bugfix-.log.md"));
    }

    #[test]
    fn is_bugfix_log_rejects_unrelated() {
        assert!(!is_bugfix_log("review.md"));
        assert!(!is_bugfix_log("consolidated-main.md"));
        assert!(!is_bugfix_log("bugfix.md"));
        assert!(!is_bugfix_log("bugfix-main.md"));
    }

    #[test]
    fn is_bugfix_log_rejects_invalid_branch_chars() {
        assert!(!is_bugfix_log("bugfix-../../evil.log.md"));
        assert!(!is_bugfix_log("bugfix-has spaces.log.md"));
        assert!(!is_bugfix_log("bugfix-feat/branch.log.md"));
        assert!(!is_bugfix_log("bugfix-a\x00b.log.md"));
    }

    #[test]
    fn bugfix_log_path_returns_err_for_empty_branch() {
        let dir = Path::new("/tmp");
        assert!(bugfix_log_path(dir, "").is_err());
    }

    #[test]
    fn bugfix_log_path_returns_err_for_invalid_chars() {
        let dir = Path::new("/tmp");
        assert!(bugfix_log_path(dir, "feat/branch").is_err());
        assert!(bugfix_log_path(dir, "../escape").is_err());
        assert!(bugfix_log_path(dir, "has spaces").is_err());
        assert!(bugfix_log_path(dir, "ok\x00null").is_err());
    }

    #[test]
    fn bugfix_log_path_returns_ok_for_valid_branch() {
        let dir = Path::new("/tmp");
        let result = bugfix_log_path(dir, "main").unwrap();
        assert_eq!(result, dir.join("bugfix-main.log.md"));
        assert!(bugfix_log_path(dir, "feature-123_test").is_ok());
    }

    #[test]
    fn migration_returns_branch_log_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bugfix-main.log.md"), "existing content").unwrap();
        let result = read_bugfix_log_with_migration(dir.path(), "main").unwrap();
        assert_eq!(result, "existing content");
    }

    #[test]
    fn migration_returns_empty_when_neither_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_bugfix_log_with_migration(dir.path(), "main").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn migration_copies_legacy_content_to_branch_log() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("bugfix.log.md");
        std::fs::write(&legacy, "legacy history").unwrap();

        let result = read_bugfix_log_with_migration(dir.path(), "main").unwrap();
        assert_eq!(result, "legacy history");

        let branch = std::fs::read_to_string(dir.path().join("bugfix-main.log.md")).unwrap();
        assert_eq!(branch, "legacy history");

        // Legacy file preserved for other branches (copy-on-read)
        assert!(legacy.exists());
    }

    #[test]
    fn migration_returns_empty_when_legacy_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bugfix.log.md"), "").unwrap();

        let result = read_bugfix_log_with_migration(dir.path(), "main").unwrap();
        assert_eq!(result, "");
        assert!(!dir.path().join("bugfix-main.log.md").exists());
    }

    #[test]
    fn migration_preserves_legacy_for_multiple_branches() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("bugfix.log.md");
        std::fs::write(&legacy, "shared history").unwrap();

        let r1 = read_bugfix_log_with_migration(dir.path(), "branch-a").unwrap();
        assert_eq!(r1, "shared history");

        let r2 = read_bugfix_log_with_migration(dir.path(), "branch-b").unwrap();
        assert_eq!(r2, "shared history");

        assert!(legacy.exists());
        assert!(dir.path().join("bugfix-branch-a.log.md").exists());
        assert!(dir.path().join("bugfix-branch-b.log.md").exists());
    }
}
