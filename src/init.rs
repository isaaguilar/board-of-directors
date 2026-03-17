use crate::claude_cli;
use crate::config::{
    self, Backend, BugfixConfig, Config, ConsolidateConfig, ModelEntry, ReviewConfig,
};
use regex::Regex;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;

/// Run `bod init`.
/// - `global`: write to ~/.config/board-of-directors/.bodrc.toml
/// - `reconfigure`: skip the "overwrite?" prompt
/// - `repo_root`: required when `global` is false (repo-scoped config, stored outside the repo)
pub fn run(global: bool, reconfigure: bool, repo_root: Option<&Path>) -> Result<(), String> {
    let (config_exists, config_path_display, load_existing): (
        bool,
        String,
        Box<dyn Fn() -> Config>,
    ) = if global {
        (
            config::global_config_exists(),
            config::global_config_path().display().to_string(),
            Box::new(config::load_global),
        )
    } else {
        let root =
            repo_root.ok_or("Not inside a git repository. Use --global for global config.")?;
        (
            config::local_config_exists(root),
            config::local_config_path(root).display().to_string(),
            {
                let root = root.to_path_buf();
                Box::new(move || try_load_local(&root).unwrap_or_default())
            },
        )
    };

    if config_exists && !reconfigure {
        let current = load_existing();
        println!("A configuration already exists at:");
        println!("  {}\n", config_path_display);
        print_config(&current);
        println!();

        if !prompt_yes_no("Do you want to overwrite the current settings?")? {
            println!("Keeping existing configuration.");
            return Ok(());
        }
        println!();
    }

    // Pick backend first (determines available models)
    println!("-- Backend --");
    let backend = prompt_backend()?;
    if backend == Backend::ClaudeCode {
        println!();
        claude_cli::print_permissions_warning();
    }
    println!();

    let models = discover_models_for_backend(&backend)?;
    println!("Available models:\n");
    for (i, model) in models.iter().enumerate() {
        println!("  [{}] {}", i + 1, model);
    }
    println!();

    // Pick 3 review models
    println!("-- Review Models --");
    println!("Pick 3 models to run as independent code reviewers.\n");
    let mut review_models: Vec<ModelEntry> = Vec::new();
    let mut used_codenames: Vec<String> = Vec::new();
    for i in 1..=3 {
        let idx = prompt_model_choice(&format!("Review model #{}", i), &models)?;
        let model = &models[idx];
        let default_cn = derive_codename(model, &used_codenames);
        let codename =
            prompt_string_with_default(&format!("Codename for '{}'", model), &default_cn)?;
        // Sanitize codename: only allow filesystem-safe characters [a-zA-Z0-9_-].
        let codename = sanitize_codename(&codename)?;
        if codename == "consolidated" || codename.starts_with("consolidated-") {
            return Err(format!(
                "Codename '{}' is reserved (conflicts with consolidated report filenames). \
                 Choose a different codename.",
                codename
            ));
        }
        used_codenames.push(codename.clone());
        review_models.push(ModelEntry {
            codename,
            model: model.clone(),
        });
        println!();
    }

    // Pick consolidation model
    println!("-- Consolidation Model --");
    let idx = prompt_model_choice("Model for consolidating reviews", &models)?;
    let consolidate_model = models[idx].clone();
    println!();

    // Pick bugfix model
    println!("-- Bugfix Model --");
    let idx = prompt_model_choice("Model for applying fixes", &models)?;
    let bugfix_model = models[idx].clone();
    println!();

    let new_config = Config {
        backend,
        review: ReviewConfig {
            models: review_models,
        },
        consolidate: ConsolidateConfig {
            model: consolidate_model,
        },
        bugfix: BugfixConfig {
            model: bugfix_model,
        },
    };

    println!("Configuration:\n");
    print_config(&new_config);
    println!();

    if global {
        config::write_global(&new_config)?;
        println!("Saved to {}", config::global_config_path().display());
    } else {
        let root = repo_root.unwrap();
        config::write_local(&new_config, root)?;
        println!("Saved to {}", config::local_config_path(root).display());
    }

    Ok(())
}

fn try_load_local(repo_root: &Path) -> Option<Config> {
    let path = config::local_config_path(repo_root);
    if !path.exists() {
        return None;
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            if let Ok(config) = toml::from_str::<Config>(&content) {
                return Some(config);
            }
        }
        Err(_) => return None,
    }

    None
}

fn print_config(config: &Config) {
    println!("  Backend:     {}", config.backend);
    println!("  Review models:");
    for m in &config.review.models {
        println!("    {} -> {}", m.codename, m.model);
    }
    println!("  Consolidation: {}", config.consolidate.model);
    println!("  Bugfix:        {}", config.bugfix.model);
}

fn prompt_backend() -> Result<Backend, String> {
    println!("Which CLI backend should bod use?\n");
    println!("  [1] Copilot CLI  (copilot)");
    println!("  [2] Claude Code  (claude)\n");

    loop {
        print!("Backend (1-2): ");
        io::stdout()
            .flush()
            .map_err(|e| format!("IO error: {}", e))?;

        let mut input = String::new();
        let bytes = io::stdin()
            .lock()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;
        if bytes == 0 {
            return Err("Unexpected end of input".to_string());
        }

        match input.trim() {
            "1" => {
                println!("  -> Copilot CLI");
                return Ok(Backend::Copilot);
            }
            "2" => {
                println!("  -> Claude Code");
                return Ok(Backend::ClaudeCode);
            }
            other => {
                eprintln!("  Invalid choice '{}'. Enter 1 or 2.", other);
            }
        }
    }
}

/// Discover models appropriate for the selected backend.
fn discover_models_for_backend(backend: &Backend) -> Result<Vec<String>, String> {
    match backend {
        Backend::Copilot => discover_copilot_models(),
        Backend::ClaudeCode => {
            // Verify the claude binary is installed before proceeding
            let version_check = Command::new("claude")
                .arg("--version")
                .output()
                .map_err(|e| {
                    format!(
                        "Failed to run 'claude --version': {}. \
                         Is the Claude Code CLI installed?",
                        e
                    )
                })?;
            if !version_check.status.success() {
                return Err(
                    "The 'claude' CLI is installed but 'claude --version' failed. \
                     Please verify your Claude Code installation."
                        .to_string(),
                );
            }

            // Verify required flags are supported using the same logic as the
            // async check in claude_cli::verify_disallowed_tools_support().
            let help_output = Command::new("claude")
                .arg("--help")
                .output()
                .map_err(|e| format!("Failed to run 'claude --help': {}", e))?;
            let help_stdout = String::from_utf8_lossy(&help_output.stdout);
            let help_stderr = String::from_utf8_lossy(&help_output.stderr);
            claude_cli::check_required_flags(&help_stdout, &help_stderr)?;

            Ok(config::claude_code_model_ids()
                .iter()
                .map(|s| s.to_string())
                .collect())
        }
    }
}

/// Discover models by parsing `copilot --help`.
fn discover_copilot_models() -> Result<Vec<String>, String> {
    println!("Discovering available models from copilot...\n");

    let output = Command::new("copilot")
        .arg("--help")
        .output()
        .map_err(|e| format!("Failed to run 'copilot --help': {}", e))?;

    let help_text = String::from_utf8_lossy(&output.stdout).to_string();

    let models = parse_model_choices(&help_text);

    if models.is_empty() {
        eprintln!("Warning: could not parse models from copilot --help. Using fallback list.");
        return Ok(fallback_copilot_models());
    }

    Ok(models)
}

fn parse_model_choices(help_text: &str) -> Vec<String> {
    // Find the --model line specifically, then extract choices from it.
    // The choices may span multiple continuation lines.
    let full = help_text.replace('\n', " ");
    let re = Regex::new(r#"--model\s+<[^>]+>\s+.*?\(choices:\s*(.*?)\)"#).unwrap();
    if let Some(caps) = re.captures(&full) {
        let choices_str = &caps[1];
        let model_re = Regex::new(r#""([^"]+)""#).unwrap();
        return model_re
            .captures_iter(choices_str)
            .map(|c| c[1].to_string())
            .collect();
    }
    Vec::new()
}

fn fallback_copilot_models() -> Vec<String> {
    vec![
        "claude-opus-4.6".to_string(),
        "claude-sonnet-4.6".to_string(),
        "claude-sonnet-4.5".to_string(),
        "claude-haiku-4.5".to_string(),
        "gemini-3-pro-preview".to_string(),
        "gpt-5.3-codex".to_string(),
        "gpt-5.2".to_string(),
        "gpt-4.1".to_string(),
    ]
}

fn prompt_yes_no(question: &str) -> Result<bool, String> {
    print!("{} (y/n): ", question);
    io::stdout()
        .flush()
        .map_err(|e| format!("IO error: {}", e))?;

    let mut input = String::new();
    let bytes = io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|e| format!("Failed to read input: {}", e))?;
    if bytes == 0 {
        return Err("Unexpected end of input".to_string());
    }

    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn prompt_model_choice(label: &str, models: &[String]) -> Result<usize, String> {
    loop {
        print!("{} (1-{}): ", label, models.len());
        io::stdout()
            .flush()
            .map_err(|e| format!("IO error: {}", e))?;

        let mut input = String::new();
        let bytes = io::stdin()
            .lock()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;
        if bytes == 0 {
            return Err("Unexpected end of input".to_string());
        }

        let input = input.trim();
        if let Ok(n) = input.parse::<usize>()
            && n >= 1
            && n <= models.len()
        {
            println!("  -> {}", models[n - 1]);
            return Ok(n - 1);
        }

        eprintln!(
            "  Invalid choice '{}'. Enter a number from 1 to {}.",
            input,
            models.len()
        );
    }
}

fn prompt_string_with_default(label: &str, default: &str) -> Result<String, String> {
    print!("{} [{}]: ", label, default);
    io::stdout()
        .flush()
        .map_err(|e| format!("IO error: {}", e))?;

    let mut input = String::new();
    let bytes = io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|e| format!("Failed to read input: {}", e))?;
    if bytes == 0 {
        return Err("Unexpected end of input".to_string());
    }

    let val = input.trim();
    if val.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(val.to_string())
    }
}

/// Sanitize a codename to contain only filesystem-safe characters `[a-zA-Z0-9_-]`.
/// Rejects codenames that produce an empty string after sanitization (e.g. `../../tmp`).
fn sanitize_codename(raw: &str) -> Result<String, String> {
    let sanitized: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    let sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.is_empty() || !sanitized.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "Codename '{}' contains no valid characters. \
             Codenames must have at least one alphanumeric character.",
            raw
        ));
    }
    Ok(sanitized)
}

/// Derive a short codename from a model ID, avoiding collisions with already-used names.
fn derive_codename(model: &str, used: &[String]) -> String {
    let base = if model.contains("opus") {
        if model.contains("fast") {
            "opus-fast"
        } else {
            "opus"
        }
    } else if model.contains("sonnet") {
        "sonnet"
    } else if model.contains("haiku") {
        "haiku"
    } else if model.starts_with("gemini") {
        "gemini"
    } else if model.contains("codex-max") {
        "codex-max"
    } else if model.contains("codex-mini") {
        "codex-mini"
    } else if model.contains("codex") {
        "codex"
    } else if model.contains("mini") {
        "mini"
    } else {
        model
    };

    let candidate = base.to_string();
    if !used.contains(&candidate) {
        return candidate;
    }

    // Append a suffix to avoid collision
    for i in 2..=9 {
        let suffixed = format!("{}{}", base, i);
        if !used.contains(&suffixed) {
            return suffixed;
        }
    }
    model.to_string()
}
