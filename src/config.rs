use crate::paths;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Copilot,
    #[serde(rename = "claude-code", alias = "claude_code")]
    ClaudeCode,
    #[serde(rename = "gemini-cli", alias = "gemini_cli")]
    GeminiCli,
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
            Self::GeminiCli => write!(f, "gemini-cli"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub review: ReviewConfig,
    pub consolidate: ConsolidateConfig,
    pub bugfix: BugfixConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub codename: String,
    pub backend: Backend,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsolidateConfig {
    pub backend: Backend,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BugfixConfig {
    pub backend: Backend,
    pub model: String,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            models: vec![
                ModelEntry {
                    codename: "opus".to_string(),
                    backend: Backend::Copilot,
                    model: "claude-opus-4.6".to_string(),
                },
                ModelEntry {
                    codename: "gemini".to_string(),
                    backend: Backend::Copilot,
                    model: "gemini-3-pro-preview".to_string(),
                },
                ModelEntry {
                    codename: "codex".to_string(),
                    backend: Backend::Copilot,
                    model: "gpt-5.3-codex".to_string(),
                },
            ],
        }
    }
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Copilot,
            model: "claude-opus-4.6".to_string(),
        }
    }
}

impl Default for BugfixConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Copilot,
            model: "gpt-5.3-codex".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyConfig {
    backend: Backend,
    review: LegacyReviewConfig,
    consolidate: LegacyStageConfig,
    bugfix: LegacyStageConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyReviewConfig {
    models: Vec<LegacyModelEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyModelEntry {
    codename: String,
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyStageConfig {
    model: String,
}

const GLOBAL_CONFIG: &str = ".bodrc.toml";

pub fn global_config_path() -> PathBuf {
    paths::app_dir().join(GLOBAL_CONFIG)
}

pub fn load(repo_root: &Path) -> Result<Config, String> {
    let local_path = local_config_path(repo_root);
    if let Some(config) = load_path(&local_path)? {
        return Ok(config);
    }

    let global_path = global_config_path();
    if let Some(config) = load_path(&global_path)? {
        return Ok(config);
    }

    Ok(Config::default())
}

pub fn load_path(path: &Path) -> Result<Option<Config>, String> {
    if !path.exists() {
        return Ok(None);
    }

    let label = path.display().to_string();
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {}", label, e))?;
    let config = parse_config_content(&content, &label)?;
    println!("Loaded config from {}", label);
    Ok(Some(config))
}

fn parse_config_content(content: &str, label: &str) -> Result<Config, String> {
    match toml::from_str::<Config>(content) {
        Ok(config) => Ok(config),
        Err(parse_error) => {
            if let Ok(legacy) = toml::from_str::<LegacyConfig>(content) {
                let _ = (
                    legacy.backend,
                    legacy.review.models.len(),
                    legacy.consolidate.model,
                    legacy.bugfix.model,
                    legacy
                        .review
                        .models
                        .iter()
                        .map(|entry| (&entry.codename, &entry.model))
                        .collect::<Vec<_>>(),
                );
                Err(format!(
                    "{} uses the old single-backend config format. Run 'bod init' to rewrite it.",
                    label
                ))
            } else {
                Err(format!("Failed to parse {}: {}", label, parse_error))
            }
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

pub fn local_config_path(repo_root: &Path) -> PathBuf {
    paths::repo_config_path(repo_root)
}

impl Config {
    pub fn used_backends(&self) -> Vec<Backend> {
        let mut used = Vec::new();
        for entry in &self.review.models {
            push_unique_backend(&mut used, entry.backend);
        }
        push_unique_backend(&mut used, self.consolidate.backend);
        push_unique_backend(&mut used, self.bugfix.backend);
        used.sort();
        used
    }
}

fn push_unique_backend(backends: &mut Vec<Backend>, backend: Backend) {
    if !backends.contains(&backend) {
        backends.push(backend);
    }
}

pub fn validate_models_for_backend(config: &Config) -> Result<(), String> {
    let mut unsafe_codenames = Vec::new();
    for entry in &config.review.models {
        if entry.codename.is_empty()
            || !entry
                .codename
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || !entry.codename.chars().any(|c| c.is_ascii_alphanumeric())
        {
            unsafe_codenames.push(entry.codename.clone());
        }
    }
    if !unsafe_codenames.is_empty() {
        unsafe_codenames.sort();
        unsafe_codenames.dedup();
        return Err(format!(
            "Invalid codename(s): {}. Codenames must contain only [a-zA-Z0-9_-] with at least one alphanumeric character. Run 'bod init' to reconfigure.",
            unsafe_codenames.join(", ")
        ));
    }

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
            "Reserved codename(s): {}. 'consolidated' conflicts with consolidated report filenames. Run 'bod init' to reconfigure.",
            reserved_codenames.join(", ")
        ));
    }

    for entry in &config.review.models {
        validate_role_model(
            entry.backend,
            &entry.model,
            &format!("reviewer '{}'", entry.codename),
        )?;
    }
    validate_role_model(
        config.consolidate.backend,
        &config.consolidate.model,
        "consolidator",
    )?;
    validate_role_model(config.bugfix.backend, &config.bugfix.model, "fixer")?;
    Ok(())
}

fn validate_role_model(backend: Backend, model: &str, role: &str) -> Result<(), String> {
    match backend {
        Backend::Copilot => {
            let suspect = [
                "opus",
                "sonnet",
                "haiku",
                "auto",
                "pro",
                "flash",
                "flash-lite",
            ];
            if suspect.contains(&model) {
                eprintln!(
                    "Warning: {} model '{}' looks like a backend-specific shorthand and may be invalid for the Copilot backend. Run 'bod init' to reconfigure.",
                    role, model
                );
            }
            Ok(())
        }
        Backend::ClaudeCode => validate_claude_model(model, role),
        Backend::GeminiCli => validate_gemini_model(model, role),
    }
}

fn validate_claude_model(model: &str, role: &str) -> Result<(), String> {
    let known = claude_code_model_ids();
    if known.contains(&model) {
        return Ok(());
    }
    if model.starts_with("claude-") {
        eprintln!(
            "Warning: {} uses unrecognized Claude model '{}'. It may work with a newer Claude CLI, but it is not in the known list.",
            role, model
        );
        return Ok(());
    }
    Err(format!(
        "Invalid model '{}' for {} on the Claude Code backend. Claude Code only supports Claude models (for example: opus, sonnet, claude-sonnet-4-6). Run 'bod init' to reconfigure.",
        model, role
    ))
}

fn validate_gemini_model(model: &str, role: &str) -> Result<(), String> {
    let known = gemini_cli_model_ids();
    if known.contains(&model) {
        return Ok(());
    }
    if model.starts_with("gemini-") {
        eprintln!(
            "Warning: {} uses unrecognized Gemini model '{}'. It may work with a newer Gemini CLI, but it is not in the known list.",
            role, model
        );
        return Ok(());
    }
    Err(format!(
        "Invalid model '{}' for {} on the Gemini CLI backend. Gemini CLI models should be one of the known aliases (auto, pro, flash, flash-lite) or start with 'gemini-'. Run 'bod init' to reconfigure.",
        model, role
    ))
}

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

pub fn gemini_cli_model_ids() -> &'static [&'static str] {
    &[
        "auto",
        "pro",
        "flash",
        "flash-lite",
        "gemini-2.5-pro",
        "gemini-2.5-flash",
        "gemini-2.5-flash-lite",
        "gemini-3-pro-preview",
    ]
}

fn try_normalize_claude_model(model: &str) -> Option<&'static str> {
    match model {
        "claude-opus-4.6" => Some("claude-opus-4-6"),
        "claude-sonnet-4.6" => Some("claude-sonnet-4-6"),
        "claude-sonnet-4.5" => Some("claude-sonnet-4-5"),
        "claude-haiku-4.5" => Some("claude-haiku-4-5"),
        _ => None,
    }
}

pub fn normalize_models_for_backend(config: &mut Config) {
    for entry in &mut config.review.models {
        normalize_role_model(
            entry.backend,
            &mut entry.model,
            &format!("review model '{}'", entry.codename),
        );
    }
    normalize_role_model(
        config.consolidate.backend,
        &mut config.consolidate.model,
        "consolidator model",
    );
    normalize_role_model(
        config.bugfix.backend,
        &mut config.bugfix.model,
        "fixer model",
    );
}

fn normalize_role_model(backend: Backend, model: &mut String, role: &str) {
    if backend != Backend::ClaudeCode {
        return;
    }

    let known = claude_code_model_ids();
    if known.contains(&model.as_str()) {
        return;
    }
    if let Some(normalized) = try_normalize_claude_model(model) {
        eprintln!(
            "Warning: normalizing {} '{}' -> '{}'.",
            role, model, normalized
        );
        *model = normalized.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_backends_are_unique_and_sorted() {
        let config = Config {
            review: ReviewConfig {
                models: vec![
                    ModelEntry {
                        codename: "a".to_string(),
                        backend: Backend::GeminiCli,
                        model: "flash".to_string(),
                    },
                    ModelEntry {
                        codename: "b".to_string(),
                        backend: Backend::Copilot,
                        model: "gpt-5.3-codex".to_string(),
                    },
                ],
            },
            consolidate: ConsolidateConfig {
                backend: Backend::ClaudeCode,
                model: "sonnet".to_string(),
            },
            bugfix: BugfixConfig {
                backend: Backend::Copilot,
                model: "gpt-5.2".to_string(),
            },
        };

        assert_eq!(
            config.used_backends(),
            vec![Backend::Copilot, Backend::ClaudeCode, Backend::GeminiCli]
        );
    }

    #[test]
    fn normalizes_claude_models_per_role() {
        let mut config = Config {
            review: ReviewConfig {
                models: vec![ModelEntry {
                    codename: "claude".to_string(),
                    backend: Backend::ClaudeCode,
                    model: "claude-opus-4.6".to_string(),
                }],
            },
            consolidate: ConsolidateConfig {
                backend: Backend::ClaudeCode,
                model: "claude-sonnet-4.6".to_string(),
            },
            bugfix: BugfixConfig {
                backend: Backend::GeminiCli,
                model: "flash".to_string(),
            },
        };

        normalize_models_for_backend(&mut config);

        assert_eq!(config.review.models[0].model, "claude-opus-4-6");
        assert_eq!(config.consolidate.model, "claude-sonnet-4-6");
        assert_eq!(config.bugfix.model, "flash");
    }

    #[test]
    fn rejects_non_claude_model_for_claude_backend() {
        let config = Config {
            review: ReviewConfig::default(),
            consolidate: ConsolidateConfig::default(),
            bugfix: BugfixConfig {
                backend: Backend::ClaudeCode,
                model: "gpt-5.3-codex".to_string(),
            },
        };

        let error = validate_models_for_backend(&config).unwrap_err();
        assert!(error.contains("Claude Code backend"));
    }

    #[test]
    fn rejects_non_gemini_model_for_gemini_backend() {
        let config = Config {
            review: ReviewConfig {
                models: vec![ModelEntry {
                    codename: "gem".to_string(),
                    backend: Backend::GeminiCli,
                    model: "claude-sonnet-4-6".to_string(),
                }],
            },
            consolidate: ConsolidateConfig::default(),
            bugfix: BugfixConfig::default(),
        };

        let error = validate_models_for_backend(&config).unwrap_err();
        assert!(error.contains("Gemini CLI backend"));
    }

    #[test]
    fn detects_legacy_single_backend_config() {
        let legacy = r#"
backend = "copilot"

[review]
models = [
  { codename = "opus", model = "claude-opus-4.6" },
  { codename = "gemini", model = "gemini-3-pro-preview" },
  { codename = "codex", model = "gpt-5.3-codex" },
]

[consolidate]
model = "claude-opus-4.6"

[bugfix]
model = "gpt-5.3-codex"
"#;

        let error = parse_config_content(legacy, "test.toml").unwrap_err();
        assert!(error.contains("old single-backend config format"));
    }
}
