# codex-appserver-ctl

Use this tool to manage Codex profiles and app-servers on macOS or Linux.
Use `--target` to run a command on an SSH host.
The tool is written in Rust. It does not require Python, Node.js, or ccusage.

## Requirements

- Codex CLI or the Codex desktop app.
- OpenSSH for commands that use `--target`.
- Cargo and Rust 1.85 or later, with a C linker, to build the tool.
- Network access to download Rust packages during the first build.

The compiled tool does not require Cargo at runtime.
Linux server commands require a Codex CLI that supports `app-server daemon`.

## Install

From the repository directory, run:

```sh
./install.sh
export PATH="$HOME/.local/bin:$PATH"
```

The installer builds a release binary. It installs the binary at
`~/.local/bin/codex-appserver-ctl`.
To use a different directory or an existing compiled binary, run:

```sh
./install.sh --prefix "$HOME/tools"
./install.sh --binary ./target/release/codex-appserver-ctl
```

Run the installer again after a source change.
The installer saves a different previous version as `codex-appserver-ctl.backup.*`.
It refuses to replace a symlink, a non-regular file, or a file owned by another user.

## Account profiles

Each host has its own profiles in `~/.codex/accounts/`.
The tool uses `~/.codex` on the selected host.

| Command | Function |
| --- | --- |
| `auth list` | Show saved profiles and current account state. |
| `auth current` | Show current account state. |
| `auth login NAME` | Log in and save a profile. Keep the current account. |
| `auth save NAME` | Save current authentication data as a profile. |
| `save NAME` | Short form of `auth save NAME`. |
| `auth use NAME` | Select a profile. Restart a running server. |
| `auth use` | Select a profile from a terminal menu. |

Start profile names with a letter or digit. Use letters, digits, `.`, `_`, or `-`.
Use a maximum of 128 characters.

```sh
codex-appserver-ctl auth login main --timeout 900
codex-appserver-ctl auth login work --timeout 900
codex-appserver-ctl auth list
codex-appserver-ctl auth use work
codex-appserver-ctl save work
```

Complete each device login in your browser with the required account.
Login uses a temporary `CODEX_HOME` and file storage for credentials.
A failed login does not add a profile.

Profile files contain credentials. The tool requires owned regular files with
mode `0600`. It rejects symlinks and hard links in the profile directory.
Do not add profile files to Git.

Use `--force` to replace an existing inactive profile.
The tool refuses to overwrite the active linked profile.
Saving equal data to an existing profile succeeds without `--force`.

Profile selection changes `auth.json` to a link to the selected profile.
The tool locks profile changes. It restores the previous authentication link
and current-account marker if an immediate restart fails.
If no server is running, selection succeeds without a restart.
To select a profile without a restart, run:

```sh
codex-appserver-ctl auth use work --no-restart
codex-appserver-ctl auth use work --dry-run
```

## Server commands

```sh
codex-appserver-ctl status
codex-appserver-ctl start
codex-appserver-ctl restart
codex-appserver-ctl stop
```

The tool uses the Codex daemon commands for a managed server.
On macOS, it can stop and reopen an app-hosted server in ChatGPT or Codex.
An unmanaged server requires its own launcher to restart.
Use `--force` to permit SIGKILL if a graceful stop fails.

A command that must restart its own parent server runs in a detached process.
The initial result confirms scheduling. It does not confirm completion.
Read the detached log to check the result:

```sh
codex-appserver-ctl logs
```

## Update Codex CLI

```sh
codex-appserver-ctl update --timeout 900
codex-appserver-ctl update --no-restart --timeout 900
codex-appserver-ctl update --dry-run
```

The tool downloads and runs the [official Codex installer](https://chatgpt.com/codex/install.sh).
It selects the latest stable release and installs at `~/.local/bin/codex`.
It verifies the installed CLI version. It then restarts a running server.
A download, install, or version-check failure does not trigger a restart.
An install can succeed while a restart fails. The error states this result.

Put `~/.local/bin` first in `PATH` to use the installed CLI in your shell.
This command updates the CLI. It does not update the desktop app or this tool.
On macOS, an app-hosted server restarts with the app's bundled executable.
Use `--no-restart` to install the CLI without restarting a server.

## Usage reports

```sh
codex-appserver-ctl usage
codex-appserver-ctl usage daily --last 7
codex-appserver-ctl usage monthly
codex-appserver-ctl usage session --json
codex-appserver-ctl usage --since 2026-10-01 --until 2026-10-06
```

The tool reads JSONL files from `~/.codex/sessions` and
`~/.codex/archived_sessions`. It groups token counts by model and day, month,
or session. It does not call ccusage or download prices.

Dates use the selected host's local timezone. Date limits include both dates.
`--last N` means N calendar days, including today, in every report mode.
The JSON report includes input, cached input, cache write, output, reasoning,
and total token counts. The table shows input, cached input, output, and total.

The reader converts cumulative token counts to increments.
It ignores repeated cumulative records and duplicate active/archive paths.
It handles counter resets and records that contain only the latest increment.
Reasoning tokens are part of output tokens. Do not add them to the total again.
Malformed JSON lines are skipped and counted.

Reports show recorded usage on this host. They do not show remaining account
limits, subscription charges, or usage that is absent from these files.
Reports can include multiple accounts used on the same host. They do not assign
sessions to authentication profiles.

### Cost estimates

Supply prices in USD per million tokens with `--prices FILE`.
Use each model name exactly as recorded in the logs.
For example, a `prices.json` file can contain these illustrative values:

```json
{
  "EXAMPLE_MODEL": { "input": 2.0, "cached": 0.2, "output": 8.0 }
}
```

```sh
codex-appserver-ctl usage monthly --prices ./prices.json
```

Replace the example values with the applicable rates.
The estimate uses uncached input, cached input, and output counts.
An unknown model price produces `-` in the table and `null` in JSON.
An estimate is not a billed charge.

## SSH targets

Omit `--target` to run on the current host.
Use an SSH alias or hostname as the target. SSH uses your normal configuration.

```sh
codex-appserver-ctl targets
codex-appserver-ctl status --target MY_SERVER
codex-appserver-ctl auth login work --target MY_SERVER --timeout 900
codex-appserver-ctl auth use work --target MY_SERVER
codex-appserver-ctl update --target MY_SERVER --timeout 900
codex-appserver-ctl usage daily --last 7 --target MY_SERVER
```

Remote login saves credentials on the remote host. It does not copy local
credentials to that host. Usage reads the remote host's session files.
A price file path refers to a file on the selected host.

The tool checks the remote command before execution.
If the tool is missing or the requested command is unavailable, it asks to
install this Rust version. Installation requires your confirmation in a terminal.
In a non-interactive session or a dry run, it reports the requirement and stops.

After confirmation, it sends the embedded source over SSH, builds on the target,
and installs at `~/.local/bin/codex-appserver-ctl`. It then runs the requested command.
The target requires Cargo, Rust 1.85 or later, a C linker, tar, and network access.
The tool does not install Rust or system packages for you.

## Remote control

```sh
codex-appserver-ctl remote-control start
codex-appserver-ctl remote-control pair
codex-appserver-ctl remote-control stop
codex-appserver-ctl remote-control enable
codex-appserver-ctl remote-control disable
codex-appserver-ctl remote-control status
codex-appserver-ctl remote-control bootstrap
```

`start`, `stop`, and `pair` use `codex remote-control`.
`enable` and `disable` change daemon remote-control settings.
`status` shows daemon state. `bootstrap` sets up a daemon with remote control.
Add `--target MY_SERVER` for a remote host.

## Diagnostics and logs

```sh
codex-appserver-ctl doctor
codex-appserver-ctl logs --lines 200
codex-appserver-ctl logs --follow
codex-appserver-ctl logs --file "$HOME/.codex/log/codex-app-server.log"
codex-appserver-ctl logs --unit YOUR_USER_SERVICE
```

`doctor` checks CLI availability, authentication, profiles, and command support.
`logs` selects the latest matching local log unless you specify a file or unit.
`--unit` uses the user journal on Linux.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
./install.sh
```

Tests use temporary profiles, synthetic session records, and fake installers.
They do not log in, download a Codex release, or restart a live server.
