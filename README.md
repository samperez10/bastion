# Bastion

**A persistent command center for AI coding agents in Termux.**

Bastion is a Termux-native workspace and session manager for terminal-based AI coding agents on Android. It organizes projects, tabs, and real PTY panes in a touch-friendly interface designed for portrait phone screens.

- **Persistent panes** — shells and agents continue running in a local background daemon after the interface detaches.
- **Native session resume** — project-scoped adapters preserve and resume supported Claude, Codex, and Antigravity sessions after a daemon restart.
- **Agent awareness** — Claude and Codex lifecycle hooks distinguish working, completed, and permission-waiting states.
- **Mobile terminal controls** — touch navigation, scrollback, bracketed paste, terminal mouse forwarding, themes, and pane management work inside one full-screen TUI.
- **Local by design** — state stays in Termux and daemon communication uses a local Unix socket.

> [!IMPORTANT]
> Bastion is alpha software. The first releases target ARM64 Android devices running Termux; support for Linux and other terminal platforms may be added later.

## Requirements

- Termux on an ARM64 Android device; the initial release target is Android API 26 or newer.
- `git`, `rust`, and `clang` when building from source.
- Claude, Codex, or Antigravity only when using that agent's integration. Ordinary shells work without an AI CLI.

Notification sounds use Android's native AAudio library directly. Bastion does **not** require Termux:API, `termux-notification`, tmux, a browser, or an external SQLite installation.

## Build and install

Until prebuilt releases are available, build inside Termux:

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
bastion open /path/to/project
```

Inside Bastion, create tabs and panes, run normal shell commands or supported agents, and press `Ctrl+B` to return from an attached pane to its workspace.

State and preferences are stored under:

```text
~/.local/share/termux-agent-workspace
```

Closing the TUI leaves the daemon and managed panes running. Lifecycle sounds are produced by the TUI, so they stop after Bastion is fully exited.

## Development

```sh
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## License

Licensed under either the Apache License 2.0 or the MIT License, at your option.
