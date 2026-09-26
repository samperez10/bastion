use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{IsTerminal, Read, Write},
    net::Shutdown,
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
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
    Doctor {
        /// Repair writable state, agent integrations, and a stale daemon.
        #[arg(long)]
        repair: bool,
    },
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
    /// Remove Bastion binaries and managed hooks without touching project files.
    Uninstall {
        /// Also remove Bastion's saved workspaces, sessions, preferences, and logs.
        #[arg(long)]
        purge: bool,
        /// Uninstall without an interactive confirmation.
        #[arg(long)]
        yes: bool,
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
        /// Retained for compatibility; manual checks are always fresh.
        #[arg(long, hide = true)]
        force: bool,
        /// Do not print status; intended for background refreshes.
        #[arg(long, hide = true)]
        quiet: bool,
    },
    /// Show the cached release status without making a network request.
    Status,
    /// Download, verify, and install the newest release.
    Install {
        /// Install without an interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Restore the previously installed Bastion release.
    Rollback {
        /// Roll back without an interactive confirmation.
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

const INSTALLED_BINARIES: [&str; 4] = [
    "bastion",
    "workspace-daemon",
    "termux-tui",
    "workspace-agent",
];
const AGENT_INTEGRATIONS: [(&str, &str); 3] = [
    ("claude", "claude"),
    ("codex", "codex"),
    ("antigravity", "agy"),
];
// Bump only when Bastion's managed hook definitions change. Application
// updates alone should not rewrite user configuration or rotate its backup.
const INTEGRATION_REVISION: &str = "2";

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(Command::Doctor { repair }) => doctor(&args.state_dir, repair),
        Some(Command::Theme { name }) => theme(&args.state_dir, name),
        Some(Command::Notifications { command }) => notifications(&args.state_dir, command),
        Some(Command::Update { command }) => update::command(&args.state_dir, command),
        Some(Command::Install { prefix }) => install(prefix),
        Some(Command::Uninstall { purge, yes }) => uninstall(&args.state_dir, purge, yes),
        Some(Command::Workspace { command }) => workspace(&args.state_dir, command),
        Some(Command::Daemon { command }) => daemon_command(&args.state_dir, command),
        Some(Command::Open { path }) => open(&args.state_dir, path),
        None => open(&args.state_dir, None),
    }
}

fn notifications(state_dir: &Path, command: NotificationsCommand) -> Result<()> {
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

fn daemon_command(state_dir: &Path, command: DaemonCommand) -> Result<()> {
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

fn workspace(state_dir: &Path, command: WorkspaceCommand) -> Result<()> {
    match command {
        WorkspaceCommand::Remove { path, force } => remove_workspace(state_dir, path, force),
    }
}

fn remove_workspace(state_dir: &Path, path: PathBuf, force: bool) -> Result<()> {
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
    let status = ProcessCommand::new(destination.join("bastion"))
        .args(["doctor", "--repair"])
        .status()
        .context("verify installed Bastion")?;
    if !status.success() {
        anyhow::bail!("Bastion was installed but its post-install repair check failed");
    }
    println!("\nReady: run `bastion` from any Termux directory.");
    Ok(())
}

fn uninstall(state_dir: &Path, purge: bool, yes: bool) -> Result<()> {
    let prefix = std::env::var_os("PREFIX")
        .map(PathBuf::from)
        .context("PREFIX is not set; run this command inside Termux")?;
    let bin_dir = prefix.join("bin");
    let executable = std::env::current_exe()?.canonicalize()?;
    if executable.parent() != Some(bin_dir.as_path()) {
        anyhow::bail!(
            "uninstall is available only from the release installed in {}",
            bin_dir.display()
        );
    }
    if !yes && !confirm_uninstall(state_dir, purge)? {
        println!("Uninstall cancelled.");
        return Ok(());
    }

    let stopped = stop_daemon(state_dir)?;
    let agent_bridge = bin_dir.join("workspace-agent");
    if agent_bridge.is_file() {
        for (agent, _) in AGENT_INTEGRATIONS {
            let status = ProcessCommand::new(&agent_bridge)
                .args(["uninstall", agent])
                .status()
                .with_context(|| format!("remove {agent} integration"))?;
            if !status.success() {
                anyhow::bail!(
                    "could not remove the {agent} integration; Bastion binaries were left installed"
                );
            }
        }
    }

    if purge {
        purge_state_safely(state_dir)?;
    } else {
        remove_update_artifacts(state_dir)?;
    }

    for binary in [
        "workspace-daemon",
        "termux-tui",
        "workspace-agent",
        "bastion",
    ] {
        let path = bin_dir.join(binary);
        match fs::remove_file(&path) {
            Ok(()) => println!("Removed {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }

    println!();
    println!("Bastion uninstalled.");
    if purge {
        println!("Bastion state removed. Project files were not touched.");
    } else {
        println!(
            "Saved workspaces and sessions remain in {}.",
            state_dir.display()
        );
        println!("Reinstalling Bastion will make them available again.");
    }
    if stopped > 0 {
        println!("Closed {stopped} live pane(s).");
    }
    println!("If your shell cached the command path, run `hash -r`.");
    Ok(())
}

fn confirm_uninstall(state_dir: &std::path::Path, purge: bool) -> Result<bool> {
    println!("Bastion will remove its binaries and managed agent hooks.");
    if purge {
        println!(
            "Bastion state will also be deleted: {}",
            state_dir.display()
        );
    } else {
        println!("Bastion state will be preserved: {}", state_dir.display());
    }
    println!("Workspace and project files are never removed.");
    print!("Continue? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn purge_state_safely(state_dir: &std::path::Path) -> Result<()> {
    if !state_dir.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(state_dir)?.file_type().is_symlink() {
        anyhow::bail!("refusing to purge a state directory reached through a symbolic link");
    }
    let target = state_dir
        .canonicalize()
        .context("resolve Bastion state directory")?;
    let mut protected = vec![PathBuf::from("/")];
    if let Some(home) = std::env::var_os("HOME") {
        protected.push(
            PathBuf::from(home)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from("/")),
        );
    }
    if let Some(prefix) = std::env::var_os("PREFIX") {
        protected.push(
            PathBuf::from(prefix)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from("/")),
        );
    }
    if let Ok(current) = std::env::current_dir().and_then(|path| path.canonicalize()) {
        protected.push(current);
    }
    if protected
        .iter()
        .any(|path| *path == target || path.starts_with(&target))
    {
        anyhow::bail!(
            "refusing to purge unsafe state path {}; choose the exact Bastion state directory",
            target.display()
        );
    }
    let database_path = target.join("workspace.db");
    if database_path.is_file() {
        for project in StateDb::open(&target)?.list_projects()? {
            let project = PathBuf::from(project.canonical_root);
            if project == target || project.starts_with(&target) {
                anyhow::bail!(
                    "refusing to purge {} because it contains registered workspace {}",
                    target.display(),
                    project.display()
                );
            }
        }
    }
    fs::remove_dir_all(&target)
        .with_context(|| format!("remove Bastion state {}", target.display()))?;
    Ok(())
}

fn remove_update_artifacts(state_dir: &std::path::Path) -> Result<()> {
    for name in [
        "update-rollback",
        "update-rollback.next",
        "update-rollback.previous",
    ] {
        let path = state_dir.join(name);
        if path.is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    let Ok(entries) = fs::read_dir(state_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("update-staging-")
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
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

fn theme(state_dir: &Path, name: Option<String>) -> Result<()> {
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

fn open(state_dir: &Path, path: Option<PathBuf>) -> Result<()> {
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
    match ensure_agent_integrations(state_dir, true, false, false) {
        Ok(0) => {}
        Ok(count) => eprintln!(
            "Bastion: {count} agent integration(s) need attention; run `bastion doctor --repair`."
        ),
        Err(error) => eprintln!(
            "Bastion: integration check failed: {error:#}. Run `bastion doctor --repair`."
        ),
    }
    show_first_run(state_dir, &workspace)?;
    ensure_daemon(state_dir, &workspace)?;
    if Preferences::load(state_dir)?.updates.automatic_checks {
        update::refresh_in_background(state_dir);
    }
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

fn show_first_run(state_dir: &Path, workspace: &Path) -> Result<()> {
    let mut preferences = Preferences::load(state_dir)?;
    if preferences.onboarding.completed
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
    {
        return Ok(());
    }
    println!();
    println!("╭─ BASTION · QUICK START ─────────────────────╮");
    println!("│ Persistent workspaces for coding agents     │");
    println!("├─────────────────────────────────────────────┤");
    println!("│ Enter / tap   Open the selected workspace   │");
    println!("│ +PANE         Create a managed terminal     │");
    println!("│ Ctrl+B        Return from a pane             │");
    println!("│ Settings      Themes, sounds, and updates    │");
    println!("├─────────────────────────────────────────────┤");
    println!("│ Workspace: {:<34}│", compact_path(workspace, 34));
    println!("╰─────────────────────────────────────────────╯");
    print!("Press Enter to continue… ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    preferences.onboarding.completed = true;
    preferences.save(state_dir)?;
    println!();
    Ok(())
}

fn compact_path(path: &std::path::Path, width: usize) -> String {
    let mut value = path.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(relative) = path.strip_prefix(&home) {
            value = if relative.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", relative.display())
            };
        }
    }
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= width {
        value
    } else {
        format!(
            "…{}",
            characters[characters.len() - width + 1..]
                .iter()
                .collect::<String>()
        )
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

fn doctor(state_dir: &PathBuf, repair: bool) -> Result<()> {
    prepare_development_companions()?;
    println!("BASTION DOCTOR{}", if repair { " · REPAIR" } else { "" });
    let mut issues = 0_usize;

    let termux = std::env::var_os("PREFIX")
        .is_some_and(|prefix| Path::new(&prefix) == Path::new("/data/data/com.termux/files/usr"));
    print_check(termux, "Termux environment", "run Bastion inside Termux");
    issues += usize::from(!termux);

    let arm64 = std::env::consts::ARCH == "aarch64";
    print_check(
        arm64,
        "ARM64 architecture",
        "current releases require aarch64",
    );
    issues += usize::from(!arm64);

    for tool in ["curl", "tar", "sha256sum"] {
        let available = find_command(tool).is_some();
        let hint = match tool {
            "curl" => "install with `pkg install curl`",
            "tar" => "install with `pkg install tar`",
            _ => "install with `pkg install coreutils`",
        };
        print_check(available, &format!("Command: {tool}"), hint);
        issues += usize::from(!available);
    }

    for binary in INSTALLED_BINARIES {
        let path = sibling_binary(binary);
        let healthy = executable_file(&path);
        print_check(
            healthy,
            &format!("Binary: {binary}"),
            "reinstall Bastion from its latest release",
        );
        issues += usize::from(!healthy);
    }

    if !state_dir.exists() && repair {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("create state directory {}", state_dir.display()))?;
    }
    if state_dir.exists() {
        let writable = state_dir
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o222 != 0)
            .unwrap_or(false);
        print_check(
            writable,
            &format!("State: {}", state_dir.display()),
            "fix the directory permissions or choose another --state-dir",
        );
        issues += usize::from(!writable);
    } else {
        println!("  · State: {} · created on first run", state_dir.display());
    }

    match daemon_status(state_dir) {
        Some(status) if status.revision == DAEMON_REVISION => println!(
            "  ✓ Daemon: revision {} · {} live pane(s)",
            status.revision, status.panes
        ),
        Some(status) if repair => {
            let workspace = StateDb::open(state_dir)?
                .focused_project()?
                .map(|project| PathBuf::from(project.canonical_root))
                .unwrap_or(std::env::current_dir()?);
            stop_daemon(state_dir)?;
            start_daemon(state_dir, &workspace)?;
            println!(
                "  ✓ Daemon: repaired revision {} → {} · saved agent sessions can resume",
                status.revision, DAEMON_REVISION
            );
        }
        Some(status) => {
            issues += 1;
            println!(
                "  ! Daemon: stale revision {} → {} · run `bastion doctor --repair`",
                status.revision, DAEMON_REVISION
            );
        }
        None => println!("  · Daemon: stopped · starts automatically with `bastion`"),
    }

    let integration_issues = ensure_agent_integrations(state_dir, repair, repair, true)?;
    issues += integration_issues;
    if issues == 0 {
        println!("Result: ready");
    } else if repair {
        println!("Result: {issues} item(s) still need attention");
    } else {
        println!("Result: {issues} item(s) need attention · run `bastion doctor --repair`");
    }
    Ok(())
}

fn print_check(ok: bool, label: &str, hint: &str) {
    if ok {
        println!("  ✓ {label}");
    } else {
        println!("  ! {label} · {hint}");
    }
}

fn executable_file(path: &std::path::Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn find_command(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| executable_file(path))
}

fn integration_stamp_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("integrations.json")
}

fn load_integration_stamps(state_dir: &std::path::Path) -> HashMap<String, String> {
    fs::read(integration_stamp_path(state_dir))
        .ok()
        .and_then(|value| serde_json::from_slice(&value).ok())
        .unwrap_or_default()
}

fn save_integration_stamps(
    state_dir: &std::path::Path,
    stamps: &HashMap<String, String>,
) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = integration_stamp_path(state_dir);
    let temporary = state_dir.join("integrations.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(stamps)?)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

fn ensure_agent_integrations(
    state_dir: &Path,
    install_missing: bool,
    force: bool,
    visible: bool,
) -> Result<usize> {
    let mut stamps = load_integration_stamps(state_dir);
    let mut changed = false;
    let mut issues = 0_usize;
    for (agent, command) in AGENT_INTEGRATIONS {
        if find_command(command).is_none() {
            if visible {
                println!("  · Integration: {agent} · CLI not installed");
            }
            continue;
        }
        let current = stamps
            .get(agent)
            .is_some_and(|version| version == INTEGRATION_REVISION);
        if current && !force {
            if visible {
                println!("  ✓ Integration: {agent}");
            }
            continue;
        }
        if !install_missing && !force {
            issues += 1;
            if visible {
                println!("  ! Integration: {agent} · run `bastion doctor --repair`");
            }
            continue;
        }
        let mut installer = ProcessCommand::new(sibling_binary("workspace-agent"));
        installer.args([
            "install",
            agent,
            "--state-dir",
            state_dir.to_string_lossy().as_ref(),
        ]);
        if !visible {
            installer.stdout(Stdio::null()).stderr(Stdio::null());
        }
        match installer.status() {
            Ok(status) if status.success() => {
                stamps.insert(agent.to_owned(), INTEGRATION_REVISION.to_owned());
                changed = true;
                if visible {
                    println!("  ✓ Integration: {agent} · repaired");
                }
            }
            Ok(status) => {
                issues += 1;
                if visible {
                    println!("  ! Integration: {agent} · installer exited with {status}");
                }
            }
            Err(error) => {
                issues += 1;
                if visible {
                    println!("  ! Integration: {agent} · {error}");
                }
            }
        }
    }
    if changed {
        save_integration_stamps(state_dir, &stamps)?;
    }
    Ok(issues)
}

fn ensure_daemon(state_dir: &Path, workspace: &Path) -> Result<()> {
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

fn start_daemon(state_dir: &Path, workspace: &Path) -> Result<()> {
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

fn daemon_status(state_dir: &Path) -> Option<DaemonStatus> {
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

fn print_restart_preview(state_dir: &Path) -> Result<()> {
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

fn stop_daemon(state_dir: &Path) -> Result<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("bastion-main-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn state_purge_never_removes_registered_project_files() {
        let root = test_dir("purge-safe");
        let project = root.join("project");
        let state = root.join("state");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("keep.txt"), "project data").unwrap();
        let database = StateDb::open(&state).unwrap();
        database.ensure_project(&project).unwrap();
        drop(database);

        purge_state_safely(&state).unwrap();
        assert!(!state.exists());
        assert_eq!(
            fs::read_to_string(project.join("keep.txt")).unwrap(),
            "project data"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn state_purge_refuses_a_directory_containing_a_workspace() {
        let root = test_dir("purge-refuse");
        let state = root.join("state");
        let nested_project = state.join("must-not-delete");
        fs::create_dir_all(&nested_project).unwrap();
        fs::write(nested_project.join("keep.txt"), "important").unwrap();
        let database = StateDb::open(&state).unwrap();
        database.ensure_project(&nested_project).unwrap();
        drop(database);

        let error = purge_state_safely(&state).unwrap_err();
        assert!(format!("{error:#}").contains("contains registered workspace"));
        assert_eq!(
            fs::read_to_string(nested_project.join("keep.txt")).unwrap(),
            "important"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn onboarding_paths_fit_the_compact_header() {
        let path = PathBuf::from("/a/very/long/workspace/path/that/exceeds/mobile-width");
        assert!(compact_path(&path, 24).chars().count() <= 24);
    }
}
