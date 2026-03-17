mod agents;
mod backend;
mod bugfix;
mod claude_cli;
mod config;
mod consolidate;
mod copilot_cli;
mod files;
mod git;
mod init;
mod paths;
mod review;

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
    /// Consolidate review findings into a unified report
    Consolidate,
    /// Autonomous review-fix loop until issues are resolved
    Bugfix {
        /// Maximum runtime in seconds (soft timeout -- finishes current step)
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
        /// Minimum severity to fix: critical, high, medium, low
        #[arg(long, default_value = "high")]
        severity: String,
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

    let mut config = config::load(&repo_root);
    config::normalize_models_for_backend(&mut config);
    if let Err(e) = config::validate_models_for_backend(&config) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }

    // Only run Claude-specific safety checks for commands that actually invoke agents.
    // This avoids spurious stderr output and subprocess calls for non-agent commands
    // like `bod version` or `bod init`.
    let needs_agent = matches!(
        cli.command,
        Commands::Review { .. } | Commands::Consolidate | Commands::Bugfix { .. }
    );
    if needs_agent && config.backend == config::Backend::ClaudeCode {
        if let Err(e) = claude_cli::verify_disallowed_tools_support().await {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        claude_cli::print_permissions_warning();
    }

    let result = match cli.command {
        Commands::Review { command: None } => review::run(&config).await.map(|_| ()).map_err(|e| e.to_string()),
        Commands::Review {
            command: Some(ReviewCommands::Consolidate),
        } => consolidate::run_latest(&config).await,
        Commands::Consolidate => consolidate::run(&config).await,
        Commands::Bugfix { timeout, severity } => {
            match bugfix::SeverityLevel::from_str(&severity) {
                Ok(level) => bugfix::run(timeout, level, &config).await,
                Err(e) => Err(e),
            }
        }
        Commands::Init { .. } | Commands::Version => unreachable!(),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
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
}
