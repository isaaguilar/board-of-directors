use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub review: ReviewConfig,
    pub consolidate: ConsolidateConfig,
    pub bugfix: BugfixConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ReviewConfig {
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelEntry {
    pub codename: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ConsolidateConfig {
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BugfixConfig {
    pub model: String,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            models: vec![
                ModelEntry {
                    codename: "opus".to_string(),
                    model: "claude-opus-4.6".to_string(),
                },
                ModelEntry {
                    codename: "gemini".to_string(),
                    model: "gemini-3-pro-preview".to_string(),
                },
                ModelEntry {
                    codename: "codex".to_string(),
                    model: "gpt-5.3-codex".to_string(),
                },
            ],
        }
    }
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-4.6".to_string(),
        }
    }
}

impl Default for BugfixConfig {
    fn default() -> Self {
        Self {
            model: "gpt-5.3-codex".to_string(),
        }
    }
}

const GLOBAL_CONFIG: &str = ".bodrc.toml";

pub fn global_config_path() -> PathBuf {
    paths::app_dir().join(GLOBAL_CONFIG)
}

/// Load config: repo-scoped external config > legacy repo-local .bodrc.toml > global config > defaults
pub fn load(repo_root: &Path) -> Config {
    if let Some(config) = try_load_repo_scoped(repo_root) {
        return config;
    }

    let global_path = global_config_path();
    if let Some(config) = try_load(&global_path, &global_path.to_string_lossy()) {
        return config;
    }

    Config::default()
}

/// Load config from global path only (for use when not in a git repo).
pub fn load_global() -> Config {
    let global_path = global_config_path();
    if let Some(config) = try_load(&global_path, &global_path.to_string_lossy()) {
        return config;
    }
    Config::default()
}

fn try_load(path: &Path, label: &str) -> Option<Config> {
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(content) => match toml::from_str::<Config>(&content) {
            Ok(config) => {
                println!("Loaded config from {}", label);
                Some(config)
            }
            Err(e) => {
                eprintln!("Warning: failed to parse {}: {}. Skipping.", label, e);
                None
            }
        },
        Err(e) => {
            eprintln!("Warning: failed to read {}: {}. Skipping.", label, e);
            None
        }
    }
}

pub fn write_global(config: &Config) -> Result<(), String> {
    let path = global_config_path();
    write_config(config, &path)
}

pub fn write_local(config: &Config, repo_root: &Path) -> Result<(), String> {
    let path = local_config_path(repo_root);
    write_config(config, &path)?;

    let legacy_path = legacy_local_config_path(repo_root);
    if legacy_path.exists() {
        std::fs::remove_file(&legacy_path).map_err(|e| {
            format!(
                "Wrote repo-scoped config to {} but failed to remove legacy config {}: {}",
                path.display(),
                legacy_path.display(),
                e
            )
        })?;
    }

    Ok(())
}

fn try_load_repo_scoped(repo_root: &Path) -> Option<Config> {
    maybe_migrate_legacy_local_config(repo_root);

    let local_path = local_config_path(repo_root);
    if let Some(config) = try_load(&local_path, &local_path.to_string_lossy()) {
        return Some(config);
    }

    let legacy_path = legacy_local_config_path(repo_root);
    try_load(&legacy_path, &legacy_path.to_string_lossy())
}

fn write_config(config: &Config, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;
    }
    let content =
        toml::to_string_pretty(config).map_err(|e| format!("Failed to serialize config: {}", e))?;
    std::fs::write(path, content)
        .map_err(|e| format!("Failed to write config to {}: {}", path.display(), e))?;
    Ok(())
}

fn maybe_migrate_legacy_local_config(repo_root: &Path) {
    let local_path = local_config_path(repo_root);
    if local_path.exists() {
        return;
    }

    let legacy_path = legacy_local_config_path(repo_root);
    if !legacy_path.exists() {
        return;
    }

    if let Some(parent) = local_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "Warning: failed to create repo-scoped config directory {} while migrating {}: {}",
            parent.display(),
            legacy_path.display(),
            e
        );
        return;
    }

    if let Err(e) = std::fs::copy(&legacy_path, &local_path) {
        eprintln!(
            "Warning: failed to migrate legacy repo config from {} to {}: {}",
            legacy_path.display(),
            local_path.display(),
            e
        );
        return;
    }

    match std::fs::remove_file(&legacy_path) {
        Ok(()) => {
            println!(
                "Migrated legacy repo config from {} to {}",
                legacy_path.display(),
                local_path.display()
            );
        }
        Err(e) => {
            eprintln!(
                "Warning: migrated config to {} but failed to remove legacy file {}: {}",
                local_path.display(),
                legacy_path.display(),
                e
            );
        }
    }
}

pub fn global_config_exists() -> bool {
    global_config_path().exists()
}

pub fn existing_local_config_path(repo_root: &Path) -> Option<PathBuf> {
    let local_path = local_config_path(repo_root);
    if local_path.exists() {
        return Some(local_path);
    }

    let legacy_path = legacy_local_config_path(repo_root);
    if legacy_path.exists() {
        return Some(legacy_path);
    }

    None
}

pub fn local_config_path(repo_root: &Path) -> PathBuf {
    paths::repo_config_path(repo_root)
}

pub fn legacy_local_config_path(repo_root: &Path) -> PathBuf {
    paths::legacy_repo_config_path(repo_root)
}
