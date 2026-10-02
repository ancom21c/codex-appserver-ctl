# codex-appserver-ctl

Use this command-line tool to manage Codex accounts and the app-server on macOS or Linux.
Use `--target` to run a command on an SSH host.

## Requirements

- Python 3.9 or later.
- The Codex app or Codex CLI.
- OpenSSH for commands that use `--target`.

Linux requires a Codex CLI that supports `codex app-server daemon`.
The tool uses Python standard libraries.
You do not need to install Python packages.

## Install the tool

From the repository directory, run:

```sh
./install.sh
```

The default installation path is `~/.local/bin/codex-appserver-ctl`.
If necessary, add this line to your shell configuration:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

To select a different installation directory, run:

```sh
./install.sh --prefix "$HOME/tools"
```

This command installs the tool in `~/tools/bin`.
Add that directory to `PATH`.

After you change the source, run `./install.sh` again.
The installer replaces a different installed version.
It first saves the previous version as `codex-appserver-ctl.backup.*` in the same directory.
It does not make a backup if the file contents are equal.

## Manage account profiles

An account profile is a saved copy of Codex authentication data.
Each host has its own profiles.

| Command | Function |
| --- | --- |
| `auth list` | Show saved profiles and the current account state. |
| `auth current` | Show the current account state. |
| `auth save NAME` | Save current authentication data as a profile. |
| `auth login NAME` | Log in and save a new profile. |
| `auth use NAME` | Select a profile and restart the app-server. |
| `auth use` | Show a profile selection menu. |

Use letters, digits, `.`, `_`, or `-` in profile names.
Start the name with a letter or digit.
Use a maximum of 128 characters.

```sh
codex-appserver-ctl auth list
codex-appserver-ctl auth current
codex-appserver-ctl auth save personal
codex-appserver-ctl auth use personal
```

To use the selection menu, run `auth use` in a terminal.
For scripts, specify the profile name.

### Log in and save a profile

Run:

```sh
codex-appserver-ctl auth login work --timeout 900
```

Complete the device login procedure that Codex shows.
The tool logs in through a temporary `CODEX_HOME` directory.
It uses file storage for authentication data.
After a successful login, it saves the profile.
It does not change the current account.
If login fails or reaches the timeout, it does not add a profile.

To select the new profile, run:

```sh
codex-appserver-ctl auth use work
```

The default timeout is 120 seconds.
Use `--timeout 900` if you need more time.
Use `--dry-run` to check the profile name and destination without login.

To replace an existing profile, add `--force`.
The tool refuses to replace a profile that the current authentication file links to.
Use a different profile name in that condition.

You can also log in with Codex directly.
Then run `auth save NAME` to save the current authentication data.

### Check or change a profile

```sh
codex-appserver-ctl auth use personal --dry-run
codex-appserver-ctl auth use personal --restart=false
codex-appserver-ctl auth save personal --force
```

`auth use --dry-run` checks the profile and restart target without a change.
If no app-server is running, `auth use` changes the profile without a restart.
It shows `auth_restart result=skipped reason=no-running-app-server`.
The next app-server start uses the selected profile.
`--restart=false` changes authentication data without a restart.
An app-server that continues to run can keep the previous authentication data.
`auth save --force` replaces an existing saved profile.

The tool locks account changes and replaces authentication files as one operation.
If the restart fails, the tool restores the previous authentication files.
A command inside the app-server can schedule the restart in a separate process.
For a scheduled command, read the log path in the command output.
Check that log for the result.

## Select an SSH host

Use `--target` with an SSH alias or `user@host`.
Replace `MY_SERVER` in the examples with your SSH alias.
Without `--target`, the tool runs on the current host.
The tool uses normal SSH configuration, key authentication, and host verification.

```sh
codex-appserver-ctl auth list --target MY_SERVER
codex-appserver-ctl auth use work --target MY_SERVER
codex-appserver-ctl auth use --target MY_SERVER
codex-appserver-ctl status --target user@host
codex-appserver-ctl auth use work --target=MY_SERVER --dry-run
```

The tool first checks `PATH` and `~/.local/bin` on the remote host.
If the tool is missing, it asks for permission to install.
If the remote version does not support the requested command, it asks for permission to update.

```text
Install this local version on MY_SERVER at ~/.local/bin/codex-appserver-ctl?
Install/update? [y/N]
```

Enter `y` or `yes` to permit installation and execution of the original command.
Any other answer stops the command without changes.
Without an interactive terminal, the tool stops without installation.
With `--dry-run`, the tool reports the missing installation without changes.

Installation copies the local script through SSH.
It does not download a version from GitHub or install Codex.
The remote host requires Python 3.9 or later.
The destination is `~/.local/bin/codex-appserver-ctl`.
The installer saves an existing file as `codex-appserver-ctl.backup.*` before replacement.
It refuses to replace a symbolic link or a file owned by another user.
After installation, the command uses the installed file directly.
This prevents an older version in `PATH` from taking priority for that command.
If installation fails, the original command does not run.

The command uses profiles on the remote host.
It does not send local authentication files to that host.
The command controls Codex for the SSH user.
It does not select a STAMCord deployment.

For older remote scripts, the tool reads the help text to select the previous `home` command format.
New commands require the current version on the remote host.

### Show SSH aliases

Run:

```sh
codex-appserver-ctl targets
```

The tool reads user and system SSH configuration files.
It also reads files specified by `Include`.
It shows explicit aliases in sorted order.
It excludes wildcard and negative patterns.
It does not test connections or evaluate `Host` and `Match` conditions.
It does not run `Match exec` commands.

### Install or update on a remote host

Connect to the remote host with SSH.
Then run:

```sh
git clone https://github.com/ancom21c/codex-appserver-ctl.git
cd codex-appserver-ctl
./install.sh
```

On that host, omit `--target` to manage its local Codex.

```sh
codex-appserver-ctl auth use work
codex-appserver-ctl restart
```

To update an existing installation, run these commands in its repository directory:

```sh
git pull --ff-only
./install.sh
```

An SSH command asks to update only if the requested command requires an update.
It does not check GitHub for newer versions.
For other source changes, update the remote installation with the commands above.

## Control the app-server

| Command | Function |
| --- | --- |
| `status` | Show the app-server state. |
| `start` | Start the managed daemon. |
| `restart` | Restart the app-server. |
| `stop` | Stop the app-server. |

```sh
codex-appserver-ctl status
codex-appserver-ctl start
codex-appserver-ctl restart
codex-appserver-ctl stop
codex-appserver-ctl restart --dry-run
codex-appserver-ctl restart --target MY_SERVER
```

The previous `true` command means restart.
The previous `false` command means stop.

On Linux, the tool uses official daemon commands.
On macOS, restart and stop use the official daemon first.
For an app-hosted server, the tool stops the related app.
For a restart, it opens that app again.
The tool recognizes the current Codex CLI bundle inside the ChatGPT and Codex apps.
It refuses to restart an unmanaged standalone server before it stops that server.
The `start` command starts a managed daemon, not the desktop app.

Use `--timeout N` to set the command and lock timeout.
The default is 120 seconds.
The permitted range is 1 to 900 seconds.
SSH connection timeouts use SSH configuration.

Use `--force` to permit forced termination if normal app-server termination fails.
The output field `target=home` identifies local Codex on the host that executes the command.

## Manage Remote Control

SSH selects the host that executes a command.
Codex Remote Control manages remote access to the daemon on that host.
These commands require a Codex CLI that supports the specified functions.

| Command after `remote-control` | Function |
| --- | --- |
| `start` | Enable Remote Control and start the daemon. |
| `pair` | Show a manual pairing code that expires after a short time. |
| `enable` | Enable Remote Control for the current daemon and future starts. |
| `disable` | Disable Remote Control. |
| `status` | Show official daemon version and state data as JSON. |
| `stop` | Stop the daemon. |
| `bootstrap` | Set up a managed daemon with Remote Control enabled. |

```sh
codex-appserver-ctl remote-control start
codex-appserver-ctl remote-control pair
codex-appserver-ctl remote-control enable
codex-appserver-ctl remote-control disable
codex-appserver-ctl remote-control status
codex-appserver-ctl remote-control stop
codex-appserver-ctl remote-control start --target MY_SERVER
codex-appserver-ctl remote-control pair --target MY_SERVER
codex-appserver-ctl remote-control bootstrap --target MY_SERVER
```

The `status` command shows `codex app-server daemon version` output.
It does not calculate Remote Control connection state.
The `pair` command shows the code in the terminal.
If the CLI does not support a command, the tool shows the CLI error.

The `bootstrap` command runs `codex app-server daemon bootstrap --remote-control`.
This command can change user service configuration.
To show the command without execution, run:

```sh
codex-appserver-ctl remote-control bootstrap --dry-run
```

## Check the installation

Run:

```sh
codex-appserver-ctl doctor
codex-appserver-ctl doctor --target MY_SERVER
```

The `doctor` command checks these items:

- CLI path and version.
- Authentication file owner and permissions.
- Profile file validity.
- Managed daemon state.
- Support for daemon, Remote Control, bootstrap, and device login commands.

It does not show authentication file contents.
A `FAIL` result gives exit code 1.
Missing authentication or an unavailable daemon gives a `WARN` result.
On macOS, the desktop app can use an app-server instead of a managed daemon.

## Read logs

```sh
codex-appserver-ctl logs
codex-appserver-ctl logs --follow --target MY_SERVER
codex-appserver-ctl logs --file /path/to/daemon.log --lines 200
codex-appserver-ctl logs --unit YOUR_USER_SERVICE --follow --target MY_SERVER
```

The default command shows the last 100 lines of the most recent applicable log.
It checks scheduled operation logs in `~/.codex`.
It also checks daemon and app-server `.log` files in these directories:

- `~/.codex/log`.
- `~/.codex/logs`.
- `~/.codex/app-server-control`.

Use `--follow` to continue to read the selected file.
If the tool finds no log, specify its path with `--file`.
For a Linux user service journal, specify the actual service name with `--unit`.
The tool then uses `journalctl --user`.
The tool does not infer service names or other log paths.

## Data files

The tool uses `~/.codex` on the host that executes the command.
It does not use `CODEX_HOME` to select a different data directory.

| Path | Contents |
| --- | --- |
| `~/.codex/auth.json` | Current authentication data. |
| `~/.codex/accounts/*.json` | Saved profiles with file permissions `0600`. |
| `~/.codex/current` | Current profile name. |
| `~/.codex/appserver-ctl-auth.lock` | Account change lock. |

Do not add authentication files to this repository.

## Remove the tool

Delete `bin/codex-appserver-ctl` from the installation directory.
This procedure leaves authentication data and saved profiles in place.

## Change and check the source

Change the source in `bin/codex-appserver-ctl`.
Change installation procedures in `install.sh`.

To check a change, run:

```sh
python3 -m unittest discover -s tests -v
sh -n install.sh
git diff --check
```

To install the change, run:

```sh
./install.sh
codex-appserver-ctl --help
```

Tests use temporary installation directories and simulated remote commands.
Tests do not make SSH connections, change real accounts, or restart real apps.
