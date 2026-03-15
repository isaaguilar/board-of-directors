use std::path::Path;
use tokio::process::Command;

pub fn command(prompt: &str, model: &str, repo_root: &Path, state_dir: &Path) -> Command {
    let mut command = Command::new("copilot");
    command
        .arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg(model)
        .arg("--allow-all-tools")
        .arg("--add-dir")
        .arg(repo_root)
        .arg("--add-dir")
        .arg(state_dir)
        .arg("--deny-tool=shell(git:*)")
        .arg("--no-ask-user")
        .arg("--autopilot");
    command
}
