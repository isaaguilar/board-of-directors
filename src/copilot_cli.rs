use crate::backend::DENIED_GIT_SUBCOMMANDS;
use std::path::Path;
use tokio::process::Command;

pub fn command(prompt: &str, model: &str, repo_root: &Path, state_dir: &Path) -> Command {
    let mut command = Command::new("copilot");
    // Defense-in-depth: override git config paths to prevent the agent from
    // reading user aliases or writing persistent config via indirect invocation.
    command.env("GIT_CONFIG_GLOBAL", crate::backend::NULL_DEVICE);
    command.env("GIT_CONFIG_SYSTEM", crate::backend::NULL_DEVICE);
    command
        .arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg(model)
        .arg("--allow-all")
        .arg("--add-dir")
        .arg(repo_root)
        .arg("--add-dir")
        .arg(state_dir);
    for cmd in DENIED_GIT_SUBCOMMANDS {
        command.arg(format!("--deny-tool=shell(git {})", cmd));
    }
    command.arg("--no-ask-user").arg("--autopilot");
    command
}
