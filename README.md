# Board of Directors

`bod` is built around one main workflow: `bugfix`. `bugfix` reviews your current git branch against the repo's default branch, consolidates the review results, runs the fixer, and repeats until it is done. If you only learn one command, learn `bod bugfix`.

## Install

```bash
brew tap GalleyBytes/tap
brew install bod
```

You also need at least one supported backend CLI installed and authenticated:

- `copilot`
- `claude`
- `gemini`

## First-time setup

Run:

```bash
bod init
```

`bod init` asks you to choose:

- 3 reviewers
- 1 consolidator
- 1 fixer

Each role can use a different backend and model.

## The main workflow: `bod bugfix`

Start here:

```bash
bod bugfix --timeout 3600 --severity high
```

What `bugfix` does for you:

- reviews the current branch
- consolidates the review results
- runs the fixer
- repeats until the run is clean, hits the iteration limit, or hits the timeout

Useful flags:

- `--iterations <n>` to cap the number of rounds
- `--severity <critical|high|medium|low>` to control what gets fixed
- `--prompt "..."` to add operator notes
- `--delay-start` to wait for a manual start
- `--no-open` to print the dashboard URL without opening a browser
- `--dry-run` to print the planned setup without launching agents

Examples:

```bash
bod bugfix --iterations 1 --severity low
bod bugfix --timeout 7200 --severity high --no-open
bod bugfix --dry-run
```

`bod bugfix` always prints the dashboard URL. Open it if you want the live dashboard. If you do not want `bod` to open the browser automatically, use `--no-open`.

## Other commands

Most people should start with `bugfix`, but these are available when you want more manual control:

- `bod review` runs only the reviewers for the current branch
- `bod review consolidate` consolidates the latest review round for the current branch
- `bod consolidate` lets you choose review files manually and consolidate them
- `bod version` prints the installed version

## Config

`bod init [--global] [--reconfigure]`

- `--global` writes config to `~/.config/board-of-directors/.bodrc.toml`
- without `--global`, config is repo-scoped
- `--reconfigure` skips the overwrite confirmation prompt

## Where files go

Board of Directors keeps its runtime files outside your repo:

```bash
~/.config/board-of-directors/<repo>/
```

That directory holds review files, consolidated reports, bugfix logs, and repo-scoped config.

## Agent behavior

Agents are allowed to use normal shell tools. The built-in git restriction is that they must not run `git commit` or `git push`.
