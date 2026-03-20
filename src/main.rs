mod agents;
mod backend;
mod bugfix;
mod bugfix_log;
mod bugfix_session;
mod claude_cli;
mod clear;
mod config;
mod consolidate;
mod copilot_cli;
mod files;
mod gemini_cli;
mod git;
mod init;
mod paths;
mod review;
mod rollback;
mod web;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bod",
    about = "Board of Directors -- multi-agent code review CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run parallel code reviews or consolidate the latest review run
    Review {
        #[command(subcommand)]
        command: Option<ReviewCommands>,
    },
    /// Clear stored review artifacts and bugfix logs for this repo
    Clear {
        #[command(subcommand)]
        command: Option<ClearCommands>,
    },
    /// Consolidate review findings into a unified report
    Consolidate,
    /// Autonomous review-fix loop until issues are resolved
    Bugfix {
        /// Maximum runtime in seconds (soft timeout -- finishes current step)
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
        /// Maximum number of fix iterations (exits early when no issues remain)
        #[arg(long)]
        iterations: Option<u32>,
        /// Minimum severity to fix: critical, high, medium, low
        #[arg(long, default_value = "high")]
        severity: String,
        /// User instructions appended to the bugfix session notes
        #[arg(long)]
        prompt: Option<String>,
        /// Wait for a manual start after opening the dashboard
        #[arg(long)]
        delay_start: bool,
        /// Start the dashboard without opening a browser automatically
        #[arg(long)]
        no_open: bool,
        /// Print the planned bugfix execution without launching agents or the dashboard
        #[arg(long)]
        dry_run: bool,
    },
    /// Print version information
    Version,
    /// Configure models interactively
    Init {
        /// Write to global config (~/.config/board-of-directors/.bodrc.toml)
        #[arg(short, long)]
        global: bool,
        /// Skip the "overwrite?" prompt and go straight to setup
        #[arg(short, long)]
        reconfigure: bool,
    },
}

#[derive(Subcommand)]
enum ReviewCommands {
    /// Consolidate the latest review run for the current branch
    Consolidate,
}

#[derive(Subcommand)]
enum ClearCommands {
    /// Remove stored review files, consolidated reports, and diff artifacts
    Reviews,
    /// Remove stored review artifacts and delete bugfix logs entirely
    All,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Version doesn't need a git repo
    if matches!(cli.command, Commands::Version) {
        println!("bod {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // Init: global mode doesn't need a git repo, local mode does
    if let Commands::Init {
        global,
        reconfigure,
    } = &cli.command
    {
        let repo_root = if *global {
            None
        } else {
            match git::repo_root() {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    eprintln!("Hint: use --global to configure without a git repo.");
                    std::process::exit(1);
                }
            }
        };
        if let Err(e) = init::run(*global, *reconfigure, repo_root.as_deref()) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    let repo_root = match git::repo_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if let Commands::Clear { command } = &cli.command {
        let mode = match command {
            Some(ClearCommands::Reviews) => clear::ClearMode::Reviews,
            Some(ClearCommands::All) => clear::ClearMode::All,
            None => clear::ClearMode::Default,
        };
        if let Err(e) = clear::run(&repo_root, mode) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    let mut config = match config::load(&repo_root) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };
    config::normalize_models_for_backend(&mut config);
    if let Err(e) = config::validate_models_for_backend(&config) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }

    for backend in active_backends_for_command(&cli.command, &config) {
        match backend {
            config::Backend::ClaudeCode => {
                if let Err(e) = claude_cli::verify_required_flags().await {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            config::Backend::GeminiCli => {
                if let Err(e) = gemini_cli::verify_required_flags().await {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            config::Backend::Copilot => {}
        }
    }

    let result = match cli.command {
        Commands::Review { command: None } => review::run(&config)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Commands::Review {
            command: Some(ReviewCommands::Consolidate),
        } => consolidate::run_latest(&config).await,
        Commands::Consolidate => consolidate::run(&config).await,
        Commands::Bugfix {
            timeout,
            iterations,
            severity,
            prompt,
            delay_start,
            no_open,
            dry_run,
        } => match bugfix::SeverityLevel::from_str(&severity) {
            Ok(level) => {
                bugfix::run(
                    timeout,
                    iterations,
                    level,
                    &config,
                    prompt.as_deref(),
                    delay_start,
                    no_open,
                    dry_run,
                )
                .await
            }
            Err(e) => Err(e),
        },
        Commands::Clear { .. } | Commands::Init { .. } | Commands::Version => unreachable!(),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn active_backends_for_command(
    command: &Commands,
    config: &config::Config,
) -> Vec<config::Backend> {
    let mut backends = Vec::new();
    match command {
        Commands::Review { command: None } => {
            for entry in &config.review.models {
                push_backend(&mut backends, entry.backend);
            }
        }
        Commands::Review {
            command: Some(ReviewCommands::Consolidate),
        }
        | Commands::Consolidate => {
            push_backend(&mut backends, config.consolidate.backend);
        }
        Commands::Clear { .. } => {}
        Commands::Bugfix { .. } => {
            backends = config.used_backends();
        }
        Commands::Init { .. } | Commands::Version => {}
    }
    backends.sort();
    backends
}

fn push_backend(backends: &mut Vec<config::Backend>, backend: config::Backend) {
    if !backends.contains(&backend) {
        backends.push(backend);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_review_command() {
        let cli = Cli::try_parse_from(["bod", "review"]).unwrap();

        assert!(matches!(cli.command, Commands::Review { command: None }));
    }

    #[test]
    fn parses_review_consolidate_subcommand() {
        let cli = Cli::try_parse_from(["bod", "review", "consolidate"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Review {
                command: Some(ReviewCommands::Consolidate)
            }
        ));
    }

    #[test]
    fn parses_plain_clear_command() {
        let cli = Cli::try_parse_from(["bod", "clear"]).unwrap();

        assert!(matches!(cli.command, Commands::Clear { command: None }));
    }

    #[test]
    fn parses_clear_reviews_subcommand() {
        let cli = Cli::try_parse_from(["bod", "clear", "reviews"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Clear {
                command: Some(ClearCommands::Reviews)
            }
        ));
    }

    #[test]
    fn parses_clear_all_subcommand() {
        let cli = Cli::try_parse_from(["bod", "clear", "all"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Clear {
                command: Some(ClearCommands::All)
            }
        ));
    }

    #[test]
    fn parses_bugfix_delay_start_flag() {
        let cli = Cli::try_parse_from(["bod", "bugfix", "--delay-start"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Bugfix {
                delay_start: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_bugfix_iterations_flag() {
        let cli = Cli::try_parse_from(["bod", "bugfix", "--iterations", "5"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Bugfix {
                iterations: Some(5),
                ..
            }
        ));
    }

    #[test]
    fn parses_bugfix_iterations_defaults_to_none() {
        let cli = Cli::try_parse_from(["bod", "bugfix"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Bugfix {
                iterations: None,
                ..
            }
        ));
    }

    #[test]
    fn parses_bugfix_dry_run_flag() {
        let cli = Cli::try_parse_from(["bod", "bugfix", "--dry-run"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Bugfix { dry_run: true, .. }
        ));
    }

    #[test]
    fn parses_bugfix_no_open_flag() {
        let cli = Cli::try_parse_from(["bod", "bugfix", "--no-open"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Bugfix { no_open: true, .. }
        ));
    }

    #[test]
    fn active_backends_for_review_only_uses_reviewer_backends() {
        let command = Commands::Review { command: None };
        let config = config::Config {
            review: config::ReviewConfig {
                models: vec![config::ModelEntry {
                    codename: "r1".to_string(),
                    backend: config::Backend::Copilot,
                    model: "gpt-5.3-codex".to_string(),
                }],
            },
            consolidate: config::ConsolidateConfig {
                backend: config::Backend::GeminiCli,
                model: "flash".to_string(),
            },
            bugfix: config::BugfixConfig {
                backend: config::Backend::ClaudeCode,
                model: "sonnet".to_string(),
            },
        };

        assert_eq!(
            active_backends_for_command(&command, &config),
            vec![config::Backend::Copilot]
        );
    }

    #[test]
    fn active_backends_for_consolidate_only_uses_consolidator_backend() {
        let command = Commands::Consolidate;
        let config = config::Config {
            review: config::ReviewConfig::default(),
            consolidate: config::ConsolidateConfig {
                backend: config::Backend::GeminiCli,
                model: "flash".to_string(),
            },
            bugfix: config::BugfixConfig {
                backend: config::Backend::ClaudeCode,
                model: "sonnet".to_string(),
            },
        };

        assert_eq!(
            active_backends_for_command(&command, &config),
            vec![config::Backend::GeminiCli]
        );
    }

    #[test]
    fn active_backends_for_clear_is_empty() {
        let command = Commands::Clear { command: None };
        let config = config::Config::default();

        assert!(active_backends_for_command(&command, &config).is_empty());
    }
}
