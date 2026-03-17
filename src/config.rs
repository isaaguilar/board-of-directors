use crate::paths;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Copilot,
    #[serde(rename = "claude-code", alias = "claude_code")]
    ClaudeCode,
}

impl Default for Backend {
    fn default() -> Self {
        Self::Copilot
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Copilot => write!(f, "copilot"),
            Self::ClaudeCode => write!(f, "claude-code"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub backend: Backend,
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

/// Load config: repo-scoped external config > global config > defaults
pub fn load(repo_root: &Path) -> Config {
    let local_path = local_config_path(repo_root);
    if let Some(config) = try_load(&local_path, &local_path.to_string_lossy()) {
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
    write_config(config, &path)
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

pub fn global_config_exists() -> bool {
    global_config_path().exists()
}

pub fn local_config_exists(repo_root: &Path) -> bool {
    local_config_path(repo_root).exists()
}

pub fn local_config_path(repo_root: &Path) -> PathBuf {
    paths::repo_config_path(repo_root)
}

/// Validate that all configured model IDs are compatible with the selected backend.
/// For Claude Code: models in the known list or matching `claude-` prefix are accepted.
/// Unknown `claude-` models produce a warning (they may work with newer CLI versions).
/// For Copilot: bare Claude-only names (no `-` or `.`) produce a warning since they
/// are likely misconfigured.
pub fn validate_models_for_backend(config: &Config) -> Result<(), String> {
    // Validate codenames contain only filesystem-safe characters [a-zA-Z0-9_-].
    let mut unsafe_codenames = Vec::new();
    for entry in &config.review.models {
        if entry.codename.is_empty()
            || !entry.codename.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || !entry.codename.chars().any(|c| c.is_ascii_alphanumeric())
        {
            unsafe_codenames.push(entry.codename.clone());
        }
    }
    if !unsafe_codenames.is_empty() {
        unsafe_codenames.sort();
        unsafe_codenames.dedup();
        return Err(format!(
            "Invalid codename(s): {}. Codenames must contain only [a-zA-Z0-9_-] with at \
             least one alphanumeric character. Run 'bod init' to reconfigure.",
            unsafe_codenames.join(", ")
        ));
    }

    // Check for reserved codenames that conflict with consolidated report filenames.
    let mut reserved_codenames = Vec::new();
    for entry in &config.review.models {
        if entry.codename == "consolidated" || entry.codename.starts_with("consolidated-") {
            reserved_codenames.push(entry.codename.clone());
        }
    }
    if !reserved_codenames.is_empty() {
        reserved_codenames.sort();
        reserved_codenames.dedup();
        return Err(format!(
            "Reserved codename(s): {}. 'consolidated' conflicts with consolidated report \
             filenames. Run 'bod init' to reconfigure.",
            reserved_codenames.join(", ")
        ));
    }

    if config.backend == Backend::Copilot {
        // Bare Claude shorthand names that are not valid Copilot model IDs.
        let claude_shorthands = ["opus", "sonnet", "haiku"];
        let mut suspect = Vec::new();

        let mut check_copilot = |model: &str| {
            if claude_shorthands.contains(&model) {
                suspect.push(model.to_string());
            }
        };

        for entry in &config.review.models {
            check_copilot(&entry.model);
        }
        check_copilot(&config.consolidate.model);
        check_copilot(&config.bugfix.model);

        if !suspect.is_empty() {
            suspect.sort();
            suspect.dedup();
            eprintln!(
                "Warning: model(s) {} look like bare Claude Code names and are likely invalid \
                 for the Copilot backend. The Copilot CLI may reject them at runtime. \
                 Run 'bod init' to reconfigure.",
                suspect.join(", ")
            );
        }
        return Ok(());
    }

    if config.backend != Backend::ClaudeCode {
        return Ok(());
    }

    let known = claude_code_model_ids();
    let mut invalid = Vec::new();
    let mut unknown_claude = Vec::new();

    let mut check = |model: &str| {
        if known.contains(&model) {
            return;
        }
        if model.starts_with("claude-") {
            unknown_claude.push(model.to_string());
        } else {
            invalid.push(model.to_string());
        }
    };

    for entry in &config.review.models {
        check(&entry.model);
    }
    check(&config.consolidate.model);
    check(&config.bugfix.model);

    if !unknown_claude.is_empty() {
        unknown_claude.sort();
        unknown_claude.dedup();
        eprintln!(
            "Warning: unrecognized Claude model(s): {}. \
             They may work with a newer Claude CLI but are not in the known list.",
            unknown_claude.join(", ")
        );
    }

    if !invalid.is_empty() {
        invalid.sort();
        invalid.dedup();
        return Err(format!(
            "Invalid model(s) for Claude Code backend: {}. \
             Claude Code only supports Claude models (e.g. opus, sonnet, claude-sonnet-4-6). \
             Run 'bod init' to reconfigure.",
            invalid.join(", ")
        ));
    }
    Ok(())
}

/// Canonical list of model IDs accepted by the Claude Code CLI.
/// Returns a static slice to avoid per-call allocation.
pub fn claude_code_model_ids() -> &'static [&'static str] {
    &[
        "opus",
        "sonnet",
        "haiku",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
    ]
}

/// Try to normalize a Claude model ID from dot-notation to dash-notation.
/// Returns Some(normalized) if a known mapping exists, None otherwise.
fn try_normalize_claude_model(model: &str) -> Option<&'static str> {
    match model {
        "claude-opus-4.6" => Some("claude-opus-4-6"),
        "claude-sonnet-4.6" => Some("claude-sonnet-4-6"),
        "claude-sonnet-4.5" => Some("claude-sonnet-4-5"),
        "claude-haiku-4.5" => Some("claude-haiku-4-5"),
        _ => None,
    }
}

/// Normalize model IDs for the Claude Code backend.
/// Maps dot-notation IDs to their dash equivalents (e.g. claude-opus-4.6 -> claude-opus-4-6).
/// Models that are already valid or unrecognized are left unchanged for validation to handle.
pub fn normalize_models_for_backend(config: &mut Config) {
    if config.backend != Backend::ClaudeCode {
        return;
    }

    let known = claude_code_model_ids();

    for entry in &mut config.review.models {
        if !known.contains(&entry.model.as_str()) {
            if let Some(normalized) = try_normalize_claude_model(&entry.model) {
                eprintln!(
                    "Warning: normalizing review model '{}' -> '{}'.",
                    entry.model, normalized
                );
                entry.model = normalized.to_string();
            }
        }
    }

    if !known.contains(&config.consolidate.model.as_str()) {
        if let Some(normalized) = try_normalize_claude_model(&config.consolidate.model) {
            eprintln!(
                "Warning: normalizing consolidate model '{}' -> '{}'.",
                config.consolidate.model, normalized
            );
            config.consolidate.model = normalized.to_string();
        }
    }

    if !known.contains(&config.bugfix.model.as_str()) {
        if let Some(normalized) = try_normalize_claude_model(&config.bugfix.model) {
            eprintln!(
                "Warning: normalizing bugfix model '{}' -> '{}'.",
                config.bugfix.model, normalized
            );
            config.bugfix.model = normalized.to_string();
        }
    }
}
