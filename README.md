# Bastion

**A persistent command center for AI coding agents in Termux.**

Bastion is a Termux-native workspace and session manager for terminal-based AI coding agents on Android. It organizes projects, tabs, and real PTY panes in a touch-friendly interface with responsive portrait and landscape layouts.

- **Persistent panes** — shells and agents continue running in a local background daemon after the interface detaches.
- **Native session resume and lifecycle state** — project-scoped adapters preserve supported Claude, Codex, and Antigravity sessions and report working, attention, and completion states when the agent exposes the required hooks.
- **Crash-safe local state** — versioned SQLite migrations create consistent backups, repair duplicate restore targets, and recover from a missing or damaged database when a valid snapshot exists.
- **Agent awareness** — Claude, Codex, and Antigravity lifecycle hooks distinguish working, completed, and permission-waiting states.
- **Mobile terminal controls** — touch navigation, scrollback, bracketed paste, terminal mouse forwarding, themes, and pane management work inside one full-screen TUI.
- **Local by design** — state stays in Termux and daemon communication uses a local Unix socket.

> [!IMPORTANT]
> Bastion is alpha software. The first releases target ARM64 Android devices running Termux; support for Linux and other terminal platforms may be added later.

## Requirements

- Termux on an ARM64 Android device; the initial release target is Android API 26 or newer.
- `curl`, `tar`, `sed`, and `coreutils` for verified release installation and updates. The installer can add missing Termux packages for you.
- `git`, `rust`, and `clang` when building from source.
- Claude, Codex, or Antigravity only when using that agent's integration. Ordinary shells work without an AI CLI.

Notification sounds use Android's native AAudio library directly. Bastion does **not** require Termux:API, `termux-notification`, tmux, a browser, or an external SQLite installation.

Bastion's interface includes its original palettes plus adaptations of [Tokyo Night](https://github.com/folke/tokyonight.nvim), [Catppuccin Mocha](https://github.com/catppuccin/catppuccin), [Gruvbox Dark](https://github.com/morhetz/gruvbox), [Solarized Dark and Light](https://github.com/altercation/solarized), [Dracula](https://github.com/dracula/dracula-theme), and [Nord](https://github.com/nordtheme/nord). Themes style Bastion's own chrome only; agent and shell panes retain their native terminal colors.

## Install

Install the latest ARM64 Termux release:

```sh
curl -fsSL https://raw.githubusercontent.com/samperez10/bastion/main/install.sh | sh
```

The installer checks the Termux environment, architecture, tools, writable install path, available storage, and GitHub connectivity before activation. Missing runtime packages are shown together and can be installed after one confirmation; AI agent CLIs are never installed automatically. It verifies the release checksum before replacing any executable. Supported agent CLIs already on the device are integrated automatically; agents installed later are detected on the next Bastion launch.

The installer detects an existing Bastion installation. It repairs integrations without downloading the same version, upgrades older releases, and refuses accidental downgrades. Useful automation controls are:

```sh
curl -fsSL https://raw.githubusercontent.com/samperez10/bastion/main/install.sh | BASTION_INSTALL_DEPS=1 sh
curl -fsSL https://raw.githubusercontent.com/samperez10/bastion/main/install.sh | BASTION_INSTALL_DEPS=0 sh
curl -fsSL https://raw.githubusercontent.com/samperez10/bastion/main/install.sh | BASTION_FORCE_INSTALL=1 sh
```

Set `BASTION_SKIP_INTEGRATIONS=1` to skip integration during installation. Set `BASTION_ALLOW_DOWNGRADE=1` only when intentionally installing an older release with `BASTION_VERSION`.

To build from source instead:

```sh
pkg install git rust clang
git clone https://github.com/samperez10/bastion.git
cd bastion
cargo run -p bastion -- install
```

Install integrations only for agents available on the device:

```sh
workspace-agent install claude
workspace-agent install codex
workspace-agent install antigravity
```

Claude and Codex configuration is backed up before modification. After installing the Codex integration, open `/hooks` once in Codex and trust the Bastion hooks.

## Use

Open the global workspace selector:

```sh
bastion
```

Register or focus a project anywhere in Termux storage:

```sh
bastion open .
bastion open /path/to/project
```

Use `bastion open .` from inside a project to register the current directory as a workspace and open it immediately.

Inside Bastion, create tabs and panes, run normal shell commands or supported agents, and press `Ctrl+B` to return from an attached pane to its workspace.

State and preferences are stored under:

```text
~/.local/share/termux-agent-workspace
```

Closing the TUI leaves the daemon and managed panes running. Lifecycle sounds are produced by the TUI, so they stop after Bastion is fully exited.

Check the installation or repair agent hooks and a stale daemon with:

```sh
bastion doctor
bastion doctor --repair
```

Uninstalling preserves workspaces and saved session state by default. Project files are never removed:

```sh
bastion uninstall
bastion uninstall --purge   # also delete Bastion's local state
```

## Updates

Bastion checks for releases in the background at most once every 24 hours. Automatic checks can be disabled under **Settings → Automatic update checks**. Available versions appear in Settings, where they can be installed, deferred, or skipped. Bastion never installs an update silently. Downloads are staged separately, checksum-verified, and activated only after validation. A failed activation or daemon restart restores the previous binaries automatically. The last working release is also retained locally for offline rollback.

The same controls are available from the command line:

```sh
bastion update check
bastion update status
bastion update install
bastion update install --yes
bastion update rollback
bastion update skip
```

Update checks fail safely when offline and never prevent Bastion from opening existing workspaces. If an update dependency is missing, `bastion update install` offers to install its Termux package first; `--yes` approves both that step and the update. `bastion update rollback` uses the local verified backup; it does not require GitHub or a network connection.

## Development

```sh
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## License

Licensed under either the Apache License 2.0 or the MIT License, at your option.
