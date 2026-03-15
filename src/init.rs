use crate::config::{self, BugfixConfig, Config, ConsolidateConfig, ModelEntry, ReviewConfig};
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
        let existing_path = config::existing_local_config_path(root);
        (
            existing_path.is_some(),
            existing_path
                .unwrap_or_else(|| config::local_config_path(root))
                .display()
                .to_string(),
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

    let models = discover_models()?;
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
    for path in [
        config::local_config_path(repo_root),
        config::legacy_local_config_path(repo_root),
    ] {
        if !path.exists() {
            continue;
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if let Ok(config) = toml::from_str::<Config>(&content) {
                    return Some(config);
                }
            }
            Err(_) => continue,
        }
    }

    None
}

fn print_config(config: &Config) {
    println!("  Review models:");
    for m in &config.review.models {
        println!("    {} -> {}", m.codename, m.model);
    }
    println!("  Consolidation: {}", config.consolidate.model);
    println!("  Bugfix:        {}", config.bugfix.model);
}

/// Discover models by parsing `copilot --help`.
fn discover_models() -> Result<Vec<String>, String> {
    println!("Discovering available models from copilot...\n");

    let output = Command::new("copilot")
        .arg("--help")
        .output()
        .map_err(|e| format!("Failed to run 'copilot --help': {}", e))?;

    let help_text = String::from_utf8_lossy(&output.stdout).to_string();

    // The model list appears like: --model <model>  ... (choices: "model1", "model2", ...)
    // It may span multiple lines between the opening ( and closing )
    let models = parse_model_choices(&help_text);

    if models.is_empty() {
        eprintln!("Warning: could not parse models from copilot --help. Using fallback list.");
        return Ok(fallback_models());
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

fn fallback_models() -> Vec<String> {
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
    io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|e| format!("Failed to read input: {}", e))?;

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
        io::stdin()
            .lock()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;

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
    io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|e| format!("Failed to read input: {}", e))?;

    let val = input.trim();
    if val.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(val.to_string())
    }
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
