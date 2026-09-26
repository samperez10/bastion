use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::Duration,
};
use workspace_core::{Preferences, StateDb};
use workspace_protocol::DAEMON_REVISION;

mod update;

#[derive(Parser)]
#[command(
    version,
    about = "Bastion — persistent command center for coding agents"
)]
struct Args {
    #[arg(long, default_value_os_t = default_state_dir())]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Focus or create the workspace rooted at PATH. Defaults to the current directory.
    Open { path: Option<PathBuf> },
    /// Verify that Bastion can find its state directory, daemon, and dashboard client.
    Doctor,
    /// Show or set the persisted Bastion chrome theme.
    Theme { name: Option<String> },
    /// Control local lifecycle notification sounds.
    Notifications {
        #[command(subcommand)]
        command: NotificationsCommand,
    },
    /// Check for and install verified Bastion releases.
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
    /// Build release binaries and install Bastion into Termux's $PREFIX/bin.
    Install {
        /// Installation prefix. Defaults to the Termux PREFIX environment variable.
        #[arg(long)]
        prefix: Option<PathBuf>,
    },
    /// Manage Bastion workspace records without changing project files.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Control Bastion's background pane daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand)]
enum UpdateCommand {
    /// Check GitHub for a newer Bastion release.
    Check {
        /// Ignore the 24-hour release-check cache.
        #[arg(long)]
        force: bool,
        /// Do not print status; intended for background refreshes.
        #[arg(long, hide = true)]
        quiet: bool,
    },
    /// Download, verify, and install the newest release.
    Install {
        /// Install without an interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Ignore the currently available version until a newer one is released.
    Skip,
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Remove a workspace from Bastion state. Project files are untouched.
    Remove {
        path: PathBuf,
        /// Stop any running panes in this workspace before removing its state.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum NotificationsCommand {
    /// Print whether notification sounds are enabled.
    Status,
    /// Enable completion and attention sounds.
    On,
    /// Disable all lifecycle notification sounds.
    Off,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Stop live panes, start the installed daemon, and restore saved agent sessions.
    Restart {
        /// Show what would resume or close without restarting anything.
        #[arg(long)]
        preview: bool,
    },
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/termux-agent-workspace")
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(Command::Doctor) => doctor(&args.state_dir),
        Some(Command::Theme { name }) => theme(&args.state_dir, name),
        Some(Command::Notifications { command }) => notifications(&args.state_dir, command),
        Some(Command::Update { command }) => update::command(&args.state_dir, command),
        Some(Command::Install { prefix }) => install(prefix),
        Some(Command::Workspace { command }) => workspace(&args.state_dir, command),
        Some(Command::Daemon { command }) => daemon_command(&args.state_dir, command),
        Some(Command::Open { path }) => open(&args.state_dir, path),
        None => open(&args.state_dir, None),
    }
}

fn notifications(state_dir: &PathBuf, command: NotificationsCommand) -> Result<()> {
    let mut preferences = Preferences::load(state_dir)?;
    match command {
        NotificationsCommand::Status => {}
        NotificationsCommand::On => {
            preferences.notifications.sound = true;
            preferences.save(state_dir)?;
        }
        NotificationsCommand::Off => {
            preferences.notifications.sound = false;
            preferences.save(state_dir)?;
        }
    }
    println!(
        "notification sounds: {}",
        if preferences.notifications.sound {
            "on"
        } else {
            "off"
        }
    );
    Ok(())
}

fn daemon_command(state_dir: &PathBuf, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Restart { preview } => {
            let workspace = StateDb::open(state_dir)?
                .focused_project()?
                .map(|project| PathBuf::from(project.canonical_root))
                .unwrap_or(std::env::current_dir().context("resolve current directory")?);
            print_restart_preview(state_dir)?;
            if preview {
                return Ok(());
            }
            let stopped = stop_daemon(state_dir)?;
            start_daemon(state_dir, &workspace)?;
            let restored = daemon_status(state_dir)
                .map(|status| status.panes)
                .unwrap_or(0);
            println!("Bastion daemon restarted.");
            println!("Stopped: {stopped} live pane(s)");
            println!("Restored: {restored} saved agent pane(s)");
            Ok(())
        }
    }
}

fn workspace(state_dir: &PathBuf, command: WorkspaceCommand) -> Result<()> {
    match command {
        WorkspaceCommand::Remove { path, force } => remove_workspace(state_dir, path, force),
    }
}

fn remove_workspace(state_dir: &PathBuf, path: PathBuf, force: bool) -> Result<()> {
    prepare_development_companions()?;
    let workspace = path.canonicalize().context("resolve workspace path")?;
    ensure_daemon(state_dir, &workspace)?;
    let mut command = ProcessCommand::new(sibling_binary("termux-tui"));
    command.args([
        "--state-dir",
        state_dir.to_string_lossy().as_ref(),
        "remove-workspace",
        workspace.to_string_lossy().as_ref(),
    ]);
    if force {
        command.arg("--force");
    }
    let status = command.status().context("remove workspace")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("workspace removal exited with {status}")
    }
}

fn install(prefix: Option<PathBuf>) -> Result<()> {
    let workspace = workspace_root()?;
    let prefix = prefix
        .or_else(|| std::env::var_os("PREFIX").map(PathBuf::from))
        .context("PREFIX is not set; run this from Termux or pass --prefix <path>")?;
    let destination = prefix.join("bin");
    fs::create_dir_all(&destination)
        .with_context(|| format!("create installer destination {}", destination.display()))?;

    println!("Building Bastion release binaries…");
    let status = ProcessCommand::new("cargo")
        .args([
            "build",
            "--release",
            "-p",
            "bastion",
            "-p",
            "workspace-daemon",
            "-p",
            "termux-tui",
            "-p",
            "workspace-agent",
        ])
        .current_dir(&workspace)
        .status()
        .context("build Bastion release binaries")?;
    if !status.success() {
        anyhow::bail!("release build failed: {status}");
    }

    let release_dir = target_dir(&workspace).join("release");
    for binary in [
        "bastion",
        "workspace-daemon",
        "termux-tui",
        "workspace-agent",
    ] {
        let source = release_dir.join(binary);
        if !source.is_file() {
            anyhow::bail!("release binary was not produced: {}", source.display());
        }
        let target = destination.join(binary);
        let temporary = destination.join(format!(".{binary}.bastion-install"));
        fs::copy(&source, &temporary).with_context(|| format!("install {}", target.display()))?;
        fs::rename(&temporary, &target)
            .with_context(|| format!("activate {}", target.display()))?;
        println!("Installed {}", target.display());
    }
    println!("\nReady: run `bastion` from any Termux directory.");
    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    let current = std::env::current_dir().context("resolve current directory")?;
    if let Some(root) = current
        .ancestors()
        .find(|path| path.join("Cargo.toml").is_file())
    {
        return Ok(root.to_owned());
    }
    let executable = std::env::current_exe().context("resolve Bastion executable")?;
    if let Some(root) = executable
        .ancestors()
        .find(|path| path.join("Cargo.toml").is_file())
    {
        return Ok(root.to_owned());
    }
    anyhow::bail!("could not find the Bastion source workspace; run from its checkout")
}

fn target_dir(workspace: &std::path::Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        }
        None => workspace.join("target"),
    }
}

fn theme(state_dir: &PathBuf, name: Option<String>) -> Result<()> {
    prepare_development_companions()?;
    let mut command = ProcessCommand::new(sibling_binary("termux-tui"));
    command.args(["--state-dir", state_dir.to_string_lossy().as_ref(), "theme"]);
    if let Some(name) = name {
        command.arg(name);
    }
    let status = command.status().context("set Bastion theme")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("Bastion theme command exited with {status}")
    }
}

fn open(state_dir: &PathBuf, path: Option<PathBuf>) -> Result<()> {
    prepare_development_companions()?;
    let open_last_session = path.is_none();
    let database = StateDb::open(state_dir)?;
    let workspace = match path {
        Some(path) => path.canonicalize().context("resolve workspace path")?,
        None => database
            .focused_project()?
            .map(|project| PathBuf::from(project.canonical_root))
            .unwrap_or(std::env::current_dir().context("resolve current directory")?),
    };
    let project = database.ensure_project(&workspace)?;
    database.focus_project(&project)?;
    ensure_daemon(state_dir, &workspace)?;
    update::refresh_in_background(state_dir);
    let mut dashboard = ProcessCommand::new(sibling_binary("termux-tui"));
    dashboard.args(["--state-dir", state_dir.to_string_lossy().as_ref()]);
    if open_last_session {
        dashboard.arg("session");
    } else {
        dashboard.args(["dashboard", "--cwd", workspace.to_string_lossy().as_ref()]);
    }
    let status = dashboard.status().context("start Bastion dashboard")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("Bastion dashboard exited with {status}")
    }
}

/// `cargo run -p bastion` only rebuilds Bastion itself. During development the
/// sibling daemon/TUI can therefore be stale even though the launcher is new.
/// Release installs ship matched binaries and skip this branch entirely.
fn prepare_development_companions() -> Result<()> {
    let Ok(executable) = std::env::current_exe() else {
        return Ok(());
    };
    let Some(debug_dir) = executable.parent() else {
        return Ok(());
    };
    if debug_dir.file_name().and_then(|name| name.to_str()) != Some("debug") {
        return Ok(());
    }
    let Some(target_dir) = debug_dir.parent() else {
        return Ok(());
    };
    let Some(workspace_root) = target_dir.parent() else {
        return Ok(());
    };
    if !workspace_root.join("Cargo.toml").is_file() {
        return Ok(());
    }
    let status = ProcessCommand::new("cargo")
        .args([
            "build",
            "-p",
            "termux-tui",
            "-p",
            "workspace-daemon",
            "-p",
            "workspace-agent",
        ])
        .current_dir(workspace_root)
        .status()
        .context("build Bastion development companions")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("could not build Bastion development companions: {status}")
    }
}

fn doctor(state_dir: &PathBuf) -> Result<()> {
    println!("state: {}", state_dir.display());
    println!("daemon: {}", sibling_binary("workspace-daemon").display());
    println!("dashboard: {}", sibling_binary("termux-tui").display());
    match daemon_status(state_dir) {
        Some(status) => println!(
            "daemon: reachable · revision {} · {} live pane(s)",
            status.revision, status.panes
        ),
        None => println!("daemon: not running"),
    }
    Ok(())
}

fn ensure_daemon(state_dir: &PathBuf, workspace: &PathBuf) -> Result<()> {
    if let Some(status) = daemon_status(state_dir) {
        if status.revision == DAEMON_REVISION {
            return Ok(());
        }
        if status.panes == 0 {
            stop_daemon(state_dir)?;
            return start_daemon(state_dir, workspace);
        }
        anyhow::bail!(
            "Bastion was updated but its running daemon is older (revision {} → {}). It has {} live pane(s), so Bastion will not stop them automatically. Run `bastion daemon restart` when ready.",
            status.revision,
            DAEMON_REVISION,
            status.panes
        );
    }
    start_daemon(state_dir, workspace)
}

fn start_daemon(state_dir: &PathBuf, workspace: &PathBuf) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create state directory {}", state_dir.display()))?;
    ProcessCommand::new(sibling_binary("workspace-daemon"))
        .args([
            "--state-dir",
            state_dir.to_string_lossy().as_ref(),
            "--cwd",
            workspace.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start Bastion daemon")?;
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if daemon_status(state_dir).is_some() {
            return Ok(());
        }
    }
    anyhow::bail!("Bastion daemon did not become ready; run `bastion doctor`")
}

struct DaemonStatus {
    revision: u32,
    panes: usize,
    pane_details: Vec<serde_json::Value>,
}

fn daemon_status(state_dir: &PathBuf) -> Option<DaemonStatus> {
    let mut stream = UnixStream::connect(state_dir.join("workspace.sock")).ok()?;
    stream.write_all(b"{\"type\":\"status\"}\n").ok()?;
    let _ = stream.shutdown(Shutdown::Write);
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let value: serde_json::Value = serde_json::from_str(response.trim()).ok()?;
    Some(DaemonStatus {
        // A daemon from before revision negotiation is deliberately stale.
        revision: value
            .get("daemon_revision")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        pane_details: value
            .get("pane_details")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default(),
        panes: value
            .get("pane_details")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len),
    })
}

fn print_restart_preview(state_dir: &PathBuf) -> Result<()> {
    let database = StateDb::open(state_dir)?;
    let mut resumable = Vec::new();
    for project in database.list_projects()? {
        for slot in database.restorable_slots(&project)? {
            let Some(session_id) = slot.native_session_id else {
                continue;
            };
            let command = native_resume_command(&slot.agent_kind, &session_id)
                .unwrap_or_else(|| format!("{} <native resume: {}>", slot.agent_kind, session_id));
            resumable.push((
                project.canonical_root.clone(),
                slot.agent_kind,
                slot.slot_name,
                command,
            ));
        }
    }
    let saved_commands = resumable
        .iter()
        .map(|(_, _, _, command)| command.as_str())
        .collect::<HashSet<_>>();
    let status = daemon_status(state_dir);
    let closing = status
        .as_ref()
        .map(|status| {
            status
                .pane_details
                .iter()
                .filter(|pane| {
                    let resume_command = pane
                        .get("resume_command")
                        .and_then(serde_json::Value::as_str);
                    let command = pane.get("command").and_then(serde_json::Value::as_str);
                    resume_command.is_none()
                        && !command.is_some_and(|command| saved_commands.contains(command))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    println!("Bastion restart preview");
    if resumable.is_empty() {
        println!("Will resume: none");
    } else {
        println!("Will resume:");
        for (workspace, agent, slot, command) in &resumable {
            println!("  {agent}/{slot} · {workspace}");
            println!("    {command}");
        }
    }
    if closing.is_empty() {
        println!("Will close without session restore: none");
    } else {
        println!("Will close without session restore:");
        for pane in closing {
            let label = pane
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pane");
            let command = pane
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("shell");
            let workspace = pane
                .get("workspace_root")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("workspace");
            println!("  {label} · {command} · {workspace}");
        }
    }
    Ok(())
}

fn native_resume_command(agent: &str, session_id: &str) -> Option<String> {
    match agent {
        "claude" => Some(format!("claude --resume {session_id}")),
        "codex" => Some(format!("codex resume {session_id}")),
        "antigravity" => Some(format!("agy --conversation={session_id}")),
        _ => None,
    }
}

fn stop_daemon(state_dir: &PathBuf) -> Result<usize> {
    let Some(status) = daemon_status(state_dir) else {
        return Ok(0);
    };
    let pid_path = state_dir.join("workspace-daemon.pid");
    let pid = fs::read_to_string(&pid_path)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0 && is_bastion_daemon(*pid))
        // One-time upgrade path for daemons from before PID records existed.
        .or_else(find_legacy_daemon_pid)
        .context("Bastion could not identify its daemon process; run `bastion doctor`")?;
    if !is_bastion_daemon(pid) {
        anyhow::bail!(
            "refusing to stop an unverified daemon PID; run `bastion doctor` for details"
        );
    }
    unsafe {
        if libc::kill(pid, libc::SIGTERM) != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("stop Bastion daemon");
            }
        }
    }
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        if daemon_status(state_dir).is_none() {
            return Ok(status.panes);
        }
    }
    unsafe {
        if libc::kill(pid, libc::SIGKILL) != 0 {
            return Err(std::io::Error::last_os_error()).context("force-stop Bastion daemon");
        }
    }
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(100));
        if daemon_status(state_dir).is_none() {
            return Ok(status.panes);
        }
    }
    anyhow::bail!("Bastion daemon did not stop; run `bastion doctor`")
}

fn is_bastion_daemon(pid: i32) -> bool {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with("workspace-daemon"))
        })
        .unwrap_or(false)
}

fn find_legacy_daemon_pid() -> Option<i32> {
    let output = ProcessCommand::new("pgrep")
        .args(["-f", "workspace-daemon"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .find(|pid| is_bastion_daemon(*pid))
}

fn sibling_binary(name: &str) -> PathBuf {
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        let candidate = parent.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(name)
}
