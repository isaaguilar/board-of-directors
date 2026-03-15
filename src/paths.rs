use std::fs;
use std::path::{Path, PathBuf};

const APP_DIR_NAME: &str = "board-of-directors";
const REPO_CONFIG_FILE: &str = "config.toml";
const LEGACY_REPO_STATE_DIR: &str = ".bod";

pub fn app_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config").join(APP_DIR_NAME)
}

pub fn repo_scope_name(repo_root: &Path) -> String {
    let raw = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());

    sanitize_component(&raw)
}

pub fn repo_state_dir(repo_root: &Path) -> PathBuf {
    app_dir().join(repo_scope_name(repo_root))
}

pub fn ensure_repo_state_dir(repo_root: &Path) -> Result<PathBuf, String> {
    let dir = repo_state_dir(repo_root);
    fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create state directory {}: {}", dir.display(), e))?;
    Ok(dir)
}

pub fn repo_config_path(repo_root: &Path) -> PathBuf {
    repo_state_dir(repo_root).join(REPO_CONFIG_FILE)
}

pub fn legacy_repo_config_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".bodrc.toml")
}

pub fn legacy_repo_state_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(LEGACY_REPO_STATE_DIR)
}

fn sanitize_component(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            ch
        } else {
            '-'
        };

        if mapped == '-' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(mapped);
            last_dash = false;
        }
    }

    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_scope_name_sanitizes_whitespace_and_symbols() {
        assert_eq!(repo_scope_name(Path::new("/tmp/my repo!")), "my-repo");
    }

    #[test]
    fn repo_scope_name_falls_back_when_missing_file_name() {
        assert_eq!(repo_scope_name(Path::new("/")), "repo");
    }

    #[test]
    fn repo_config_path_lives_under_external_app_dir() {
        let repo_root = Path::new("/work/board-of-directors");
        assert_eq!(
            repo_config_path(repo_root),
            app_dir().join("board-of-directors").join("config.toml")
        );
    }
}
