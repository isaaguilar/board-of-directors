# Board of Directors (bod)

Board of Directors (`bod`) is a multi-agent code-review CLI that runs parallel AI reviewers, consolidates feedback, and can assist with automated fixes.

## Requirements

- At least one of the following AI CLIs installed and available in the environment:
  - Copilot CLI (`copilot`)
  - Claude Code (`claude`)
  - Gemini CLI (`gemini`)

## Installation

```bash
brew tap GalleyBytes/tap
brew install bod
```

## Commands

`bod review`
Run parallel reviews for the current branch.

`bod review consolidate`
Consolidate the latest review round for the current branch into a single report.

`bod consolidate`
Consolidate review findings (non-branch-specific).

`bod bugfix --timeout <seconds> --severity <critical|high|medium|low>`
Run the autonomous review-fix loop. When bugfix mode starts, `bod` also launches a localhost dashboard on a random port in `20000..=29999` and opens it in your browser. The dashboard shows iteration progress, the session timeout countdown, the current and next severity thresholds, rendered markdown artifacts with syntax highlighting, an editable user-notes section stored inside the bugfix log, and a cancel control that stops the run and reverts current-iteration fix changes. Example: `bod bugfix --timeout 3600 --severity high`

## Bugfix dashboard

`bod bugfix` now serves a browser UI while the loop is running. Use it to watch review progress, inspect the latest consolidated report and bugfix log, leave notes for the next consolidation pass, change the severity threshold for the next iteration, or cancel the current run.

If automatic browser launch fails, open the printed `http://127.0.0.1:<port>/` URL manually.

The localhost control plane is served by `bod`, but the browser UI currently bootstraps React, TypeScript, Tailwind, markdown rendering, and syntax highlighting from CDN assets at runtime. The CLI loop still works without that browser-side network access, but the dashboard will not render fully if those browser fetches are blocked.

`bod init [--global] [--reconfigure]`
Interactive backend/model setup for the 3 reviewers, the consolidator, and the fixer. Each role can use a different backend and model, and you can either choose from the discovered model list or type a custom model ID. `--global` writes to `~/.config/board-of-directors/.bodrc.toml`. Legacy single-backend configs are no longer accepted; re-run `bod init` to regenerate them.

`bod version`
Print version information.

## Security: backend notes

When the Claude Code backend is selected, `bod` passes `--dangerously-skip-permissions` to the `claude` CLI so that review and fix agents can run without interactive confirmation prompts. **All** git operations are blocked via `--disallowed-tools "Bash(git:*)"`. This blocks both destructive and read-only git commands to ensure the deny list is reliably enforced (per-subcommand patterns like `Bash(git commit:*)` are not documented as supported by the Claude CLI).

The Copilot backend uses per-subcommand blocking (`--deny-tool=shell(git <subcmd>)`) for the following git subcommands: `commit`, `push`, `pull`, `fetch`, `remote`, `rebase`, `reset`, `clean`, `merge`, `checkout`, `switch`, `restore`, `apply`, `cherry-pick`, `revert`, `rm`, `branch`, `tag`, `stash`, `config`, `add`, `update-index`, `mv`. Read-only git operations (`status`, `diff`, `log`) remain available under the Copilot backend.

The Gemini backend runs in headless mode with `--approval-mode yolo` and `--sandbox`. This gives non-interactive execution plus an extra containment layer, but Gemini CLI does not currently expose the same git-specific deny-list controls as Copilot or Claude, so git restrictions remain prompt-level guidance rather than hard CLI enforcement.

**Deny-list limitations:**
- The Claude deny pattern `Bash(git:*)` blocks all git invocations through the Bash tool. An LLM can bypass this via indirect invocations such as `env git commit`, `bash -c "git commit"`, or `/usr/bin/git commit`.
- Non-git destructive commands (`rm -rf`, `curl | sh`, data exfiltration) are not restricted by the deny list.
- An allow-list approach (restricting the agent to specific tools only) is not currently feasible because the agent needs general shell access for builds, tests, and file operations.

**Prompt delivery:**
- The Claude backend delivers prompts via stdin, so diff content is not visible in `ps` output.
- The Gemini backend also delivers prompts via stdin, so diff content is not visible in `ps` output.
- The Copilot backend passes prompts as CLI arguments (`-p`), which are visible in `ps aux` on multi-user systems. Avoid running the Copilot backend on shared hosts with sensitive code.

Mitigations:
- A visible warning is printed to stderr at startup when this flag is active.
- Review agent output and consolidated reports before acting on them.
- Run `bod` in an environment with limited privileges when possible (containers, CI runners).
- **Claude Code backend should be used only in sandboxed or trusted-code-only environments** due to the inherent limitations of tool-deny patterns.
- **Gemini CLI backend should be used only in sandboxed or trusted-code-only environments** until it exposes stronger non-interactive tool restrictions.
- The Copilot CLI backend does not use this flag and is not affected.
- Note: The PATH-based safe-bin and shim approach reduces risk but cannot prevent a child process from executing absolute-path binaries (e.g., /usr/bin/git) or other bypasses; for stronger containment run agents inside OS-level sandboxes (containers, seccomp, mount namespaces) or only on trusted hosts.

## State location

Runtime and configuration are stored outside the repository to keep the working tree clean:

```bash
$HOME/.config/board-of-directors/<repo-scope>/
```

Replaces `<repo-scope>` with a sanitized form of the repo directory name. 
