# codex-appserver-ctl

Use this tool to manage Codex profiles and app-servers on macOS or Linux.
Use `--target` to run a command on an SSH host.
The tool is written in Rust. It does not require Python, Node.js, or ccusage.

## Requirements

For a release binary, use:

- macOS or Linux on ARM64 or x86_64.
- `curl`, `tar`, and `sha256sum` or `shasum` to install the tool.
- HTTPS access to GitHub Releases and its download hosts.
- Codex CLI or the Codex desktop app for account and server commands.
- OpenSSH for commands that use `--target`.

Rust, Cargo, and a C linker are not required to install or run a release binary.
Linux server commands require a Codex CLI that supports `app-server daemon`.
Linux release binaries use musl. They do not require a specific glibc version.

## Install a release binary

Download the installer from the latest release. Then run it:

```sh
curl -fsSL https://github.com/ancom21c/codex-appserver-ctl/releases/latest/download/install.sh -o /tmp/codex-appserver-ctl-install.sh
sh /tmp/codex-appserver-ctl-install.sh
export PATH="$HOME/.local/bin:$PATH"
codex-appserver-ctl --version
```

The installer selects the archive for your OS and CPU.
It checks the SHA-256 digest before extraction.
It installs the binary at `~/.local/bin/codex-appserver-ctl`.

To select a version or installation directory, run:

```sh
sh /tmp/codex-appserver-ctl-install.sh --version 0.3.0
sh /tmp/codex-appserver-ctl-install.sh --prefix "$HOME/tools"
```

From a repository checkout, `./install.sh` also downloads the latest release.
Use `--source` to build the checked-out source instead.
To install an existing compiled binary, run:

```sh
./install.sh --binary ./target/release/codex-appserver-ctl
```

The installer saves a different previous version as `codex-appserver-ctl.backup.*`.
It refuses to replace a symlink, a non-regular file, or a file owned by another user.
A download or digest-check failure leaves the installed version unchanged.
It does not start a source build automatically.

## Build from source

A source build requires Cargo, Rust 1.85 or later, a C compiler and linker,
and network access to download Rust packages.
Install the OS build tools first.

On macOS, run:

```sh
xcode-select --install
```

On Debian or Ubuntu, run:

```sh
sudo apt-get update
sudo apt-get install -y build-essential curl ca-certificates
```

On Fedora, run:

```sh
sudo dnf install gcc gcc-c++ make curl ca-certificates
```

Install Rust and Cargo with the [official Rust installer](https://rust-lang.org/tools/install/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh
sh /tmp/rustup-init.sh
. "$HOME/.cargo/env"
rustc --version
cargo --version
```

If Rust is already installed with rustup, use `rustup update stable` to update it.
From the repository directory, run:

```sh
./install.sh --source
```

The build uses `Cargo.lock`. Rust packages are required at build time.
They do not add a Python, Node.js, or ccusage runtime requirement.

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

## Self-update

```sh
codex-appserver-ctl update --dry-run
codex-appserver-ctl update --timeout 900
```

Downloads and runs the upstream release's `install.sh`, installing the latest
published release in the current `PREFIX/bin` location. The release installer
verifies the archive checksum and keeps a backup. Checkout binaries must first
be installed using `./install.sh --source` or `--binary FILE`.
New PR features become available through self-update after a release is published.

## Weekly account limits

```sh
codex-appserver-ctl limits
codex-appserver-ctl limits --timeout 30
codex-appserver-ctl limits --watch
```

Checks all saved accounts and the active account concurrently through isolated
Codex app-servers. Displays weekly used/remaining percentages and reset times
in the local timezone. `*` marks the active account. Terminal output shows an
animated progress indicator until every account finishes. The bordered table
appears immediately with the previous quota metrics (or placeholders on first
use), labeled REFRESHING, then is replaced when all checks complete. REMAINING
uses a gauge, RESET IN shows a countdown, and LAST UPDATED records each account's
last successful check. Failed refreshes retain previous metrics with STALE status
and their original timestamps. A private 0600 `~/.codex/appserver-ctl-limits.json`
cache stores quota metrics, timestamps, and stable user/workspace identifiers,
without tokens. Replacing an account invalidates its previous metrics; token
rotation keeps them. Unknown identities and older unbound caches are not reused.
An unmanaged active account is labeled `(active)` separately from saved profiles.
Narrow terminals show a compact table with reset and timestamp details below it.
Frames that no longer fit after a resize are appended without erasing scrollback.
`--watch` enables
keyboard refresh (`r`) and quit (`q`); redirected output remains a plain table.
Identical credential files share one request.

Concurrent `limits` invocations on the same account store serialize their refresh
batches, while accounts within each batch are still checked in parallel. A queued
invocation shows cached metrics and a cancellable waiting indicator, then reloads
credentials and cache after the preceding refresh finishes. This prevents duplicate
token rotation and an older batch overwriting a newer cache. The refresh lock is
released before final terminal output and while `--watch` waits for keyboard input.
CURRENT is rechecked before displaying the result, including changes to aliases.
Managed token refreshes are saved
only if the original files still match, without switching the active account or
restarting an existing server. Unsupported weekly windows show N/A; individual
failures remain visible and produce a nonzero exit status. Ctrl-C cancels checks,
including waits for the authentication lock, and exits with status 130. Token
rotation is still saved after cancellation or a quota request failure. If a lock
or concurrent profile edit prevents saving, the error reports a private recovery
`auth.json` path instead of discarding the rotated credentials. Inspect it and
reconcile it with the intended account before retrying; never overwrite a profile
that was replaced during the check.

## Update Codex CLI

```sh
codex-appserver-ctl update codex --timeout 900
codex-appserver-ctl update codex --no-restart --timeout 900
codex-appserver-ctl update codex --dry-run
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

After confirmation, it sends the embedded installer over SSH.
The target downloads the release that matches the local tool version.
It verifies the archive digest and installs at `~/.local/bin/codex-appserver-ctl`.
It verifies command support again before running the requested command.
Version 0.4 changes `update` to self-update and adds `update codex` and `limits`.
Those commands require the new command signatures; a v0.3 remote must be upgraded
first, so `update` cannot accidentally restart its Codex server.

The target requires curl, tar, a SHA-256 tool, and HTTPS access to GitHub Releases.
Cargo and Rust are not required. The tool does not install system packages.
If the target cannot download the release, installation stops.
To build there instead, check out the repository and run `./install.sh --source`.

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
./install.sh --source
```

Tests require Python 3 for fake app-servers and SSH probes. The installed CLI has
no Python runtime dependency. Tests use temporary profiles, synthetic session
records, fake installers, and real pseudo-terminals. Pull requests run the same
four-platform checks as releases, without publishing.
They do not log in, download a Codex release, or restart a live server.

## Publish a release

The release workflow builds and tests four targets: macOS ARM64, macOS x86_64,
Linux ARM64 musl, and Linux x86_64 musl.
It publishes archives, SHA-256 files, and `install.sh` to GitHub Releases.

Set the package version in `Cargo.toml` and update `Cargo.lock`.
Commit the changes. Then push a matching version tag:

```sh
git tag v0.4.0
git push origin v0.4.0
```

The tag must match the package version. All four builds must pass before publication.
A manual workflow run builds and tests the binaries without publishing a release.
