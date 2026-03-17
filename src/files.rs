use crate::paths;
use std::path::{Path, PathBuf};

const BUGFIX_LOG: &str = "bugfix.log.md";

pub fn ensure_state_dir(repo_root: &Path) -> Result<PathBuf, String> {
    paths::ensure_repo_state_dir(repo_root)
}

pub fn bugfix_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(BUGFIX_LOG)
}
