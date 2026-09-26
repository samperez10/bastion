use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

#[derive(Parser)]
#[command(about = "Agent-session bridge for Termux Agent Workspace")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a managed session-restore integration without replacing other hooks.
    Install {
        #[arg(value_enum)]
        agent: Agent,
        /// Agent configuration directory. Defaults to the agent's documented location.
        #[arg(long)]
        config_dir: Option<PathBuf>,
        /// State directory used by workspace-daemon and termux-tui.
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
    },
    /// Remove only Bastion-managed hooks and preserve all other agent configuration.
    Uninstall {
        #[arg(value_enum)]
        agent: Agent,
        /// Agent configuration directory. Defaults to the agent's documented location.
        #[arg(long)]
        config_dir: Option<PathBuf>,
    },
    /// Read Claude's SessionStart JSON from stdin and report its native session ID.
    ReportClaude {
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
    },
    /// Report a Claude lifecycle transition for a Bastion-managed pane.
    ReportClaudeState {
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
        #[arg(long, value_enum)]
        state: LifecycleState,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Read Codex's SessionStart JSON from stdin and report its native session ID.
    ReportCodex {
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
    },
    /// Report a Codex lifecycle transition for a Bastion-managed pane.
    ReportCodexState {
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
        #[arg(long, value_enum)]
        state: LifecycleState,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Read Antigravity's PreInvocation JSON from stdin and report conversationId.
    ReportAntigravity {
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
    },
    /// Read Antigravity's ask_permission tool hook and report attention.
    ReportAntigravityAttention {
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
    },
    /// Read Antigravity's Stop hook and report completion when fully idle.
    ReportAntigravityStop {
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
        #[arg(long, default_value = "primary")]
        slot: String,
    },
}

#[derive(Clone, ValueEnum)]
enum Agent {
    Claude,
    Codex,
    Antigravity,
}

#[derive(Clone, Copy, ValueEnum)]
enum LifecycleState {
    Idle,
    Working,
    Attention,
    Done,
}

impl LifecycleState {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Attention => "attention",
            Self::Done => "done",
        }
    }
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Install {
            agent,
            config_dir,
            state_dir,
            slot,
        } => match agent {
            Agent::Claude => install_claude(
                config_dir.unwrap_or_else(claude_config_dir),
                state_dir,
                &slot,
            ),
            Agent::Codex => install_codex(
                config_dir.unwrap_or_else(codex_config_dir),
                state_dir,
                &slot,
            ),
            Agent::Antigravity => install_antigravity(
                config_dir.unwrap_or_else(antigravity_config_dir),
                state_dir,
                &slot,
            ),
        },
        Command::Uninstall { agent, config_dir } => match agent {
            Agent::Claude => uninstall_json_hooks(
                config_dir
                    .unwrap_or_else(claude_config_dir)
                    .join("settings.json"),
                &["report-claude"],
                "Claude",
            ),
            Agent::Codex => uninstall_json_hooks(
                config_dir
                    .unwrap_or_else(codex_config_dir)
                    .join("hooks.json"),
                &["report-codex"],
                "Codex",
            ),
            Agent::Antigravity => uninstall_antigravity(
                config_dir
                    .unwrap_or_else(antigravity_config_dir)
                    .join("hooks.json"),
            ),
        },
        Command::ReportClaude { state_dir, slot } => report_claude(state_dir, &slot),
        Command::ReportClaudeState {
            state_dir,
            slot,
            state,
            reason,
        } => report_agent_state(state_dir, "claude", &slot, state, reason.as_deref()),
        Command::ReportCodex { state_dir, slot } => report_codex(state_dir, &slot),
        Command::ReportCodexState {
            state_dir,
            slot,
            state,
            reason,
        } => report_agent_state(state_dir, "codex", &slot, state, reason.as_deref()),
        Command::ReportAntigravity { state_dir, slot } => {
            report_antigravity(state_dir, &slot, AntigravityHook::PreInvocation)
        }
        Command::ReportAntigravityAttention { state_dir, slot } => {
            report_antigravity(state_dir, &slot, AntigravityHook::Attention)
        }
        Command::ReportAntigravityStop { state_dir, slot } => {
            report_antigravity(state_dir, &slot, AntigravityHook::Stop)
        }
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn default_state_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".local/share/termux-agent-workspace")
}

fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".claude")
        })
}

fn codex_config_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".codex")
        })
}

fn antigravity_config_dir() -> PathBuf {
    std::env::var_os("ANTIGRAVITY_CLI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".gemini/config")
        })
}

fn install_claude(config_dir: PathBuf, state_dir: PathBuf, slot: &str) -> Result<()> {
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("create Claude config directory {}", config_dir.display()))?;
    let settings_path = config_dir.join("settings.json");
    let mut settings = if settings_path.exists() {
        let input = fs::read_to_string(&settings_path)
            .with_context(|| format!("read {}", settings_path.display()))?;
        serde_json::from_str(&input)
            .with_context(|| format!("parse {} as JSON", settings_path.display()))?
    } else {
        json!({})
    };
    let root = settings
        .as_object_mut()
        .context("Claude settings must be a JSON object")?;
    let hooks = object_field(root, "hooks", "Claude settings key `hooks`")?;
    // Older Bastion builds installed an `Interrupt` hook, but Claude does not
    // expose that event. Remove only our obsolete command and leave any other
    // user configuration untouched.
    remove_workspace_agent_hooks(hooks, "Interrupt", "report-claude-state");
    let session_start = array_field(hooks, "SessionStart", "Claude SessionStart hooks")?;

    let executable = std::env::current_exe().context("locate workspace-agent executable")?;
    let slot_default = shell_slot_default(slot)?;
    let command = managed_hook_command(format!(
        "{} report-claude --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&state_dir.to_string_lossy()),
        slot_default
    ));
    let updated = replace_workspace_agent_hook(session_start, "report-claude", &command);
    if !updated {
        session_start.push(json!({
            "matcher": "startup|resume|clear|compact|fork",
            "hooks": [{"type": "command", "command": command}]
        }));
    }

    install_claude_state_hook(
        hooks,
        "UserPromptSubmit",
        "working",
        None,
        &executable,
        &state_dir,
        slot_default,
    )?;
    install_claude_state_hook(
        hooks,
        "PermissionRequest",
        "attention",
        Some("permission"),
        &executable,
        &state_dir,
        slot_default,
    )?;
    install_claude_state_hook(
        hooks,
        "Stop",
        "done",
        None,
        &executable,
        &state_dir,
        slot_default,
    )?;
    install_claude_state_hook(
        hooks,
        "SessionEnd",
        "idle",
        None,
        &executable,
        &state_dir,
        slot_default,
    )?;

    if settings_path.exists() {
        let backup = settings_path.with_file_name("settings.json.workspace-agent-backup");
        fs::copy(&settings_path, &backup)
            .with_context(|| format!("back up {}", settings_path.display()))?;
        println!("Backed up existing settings to {}", backup.display());
    }
    let rendered = serde_json::to_string_pretty(&settings)? + "\n";
    fs::write(&settings_path, rendered)
        .with_context(|| format!("write {}", settings_path.display()))?;
    let action = if updated { "Updated" } else { "Installed" };
    println!(
        "{action} Claude SessionStart integration in {}",
        settings_path.display()
    );
    println!("State directory: {}", state_dir.display());
    Ok(())
}

fn report_claude(state_dir: PathBuf, slot: &str) -> Result<()> {
    report_json_session(state_dir, "claude", slot)
}

fn install_claude_state_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    state: &str,
    reason: Option<&str>,
    executable: &std::path::Path,
    state_dir: &std::path::Path,
    slot_default: &str,
) -> Result<()> {
    let groups = array_field(hooks, event, &format!("Claude {event} hooks"))?;
    let reason = reason
        .map(|value| format!(" --reason {}", shell_quote(value)))
        .unwrap_or_default();
    let command = managed_hook_command(format!(
        "{} report-claude-state --state {} --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"{}",
        shell_quote(&executable.to_string_lossy()),
        state,
        shell_quote(&state_dir.to_string_lossy()),
        slot_default,
        reason,
    ));
    let reporter = format!("report-claude-state --state {state}");
    if !replace_workspace_agent_hook(groups, &reporter, &command) {
        groups.push(json!({
            "hooks": [{"type": "command", "command": command, "timeout": 3}]
        }));
    }
    Ok(())
}

fn install_codex(config_dir: PathBuf, state_dir: PathBuf, slot: &str) -> Result<()> {
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("create Codex config directory {}", config_dir.display()))?;
    let hooks_path = config_dir.join("hooks.json");
    let mut settings = if hooks_path.exists() {
        let input = fs::read_to_string(&hooks_path)
            .with_context(|| format!("read {}", hooks_path.display()))?;
        serde_json::from_str(&input)
            .with_context(|| format!("parse {} as JSON", hooks_path.display()))?
    } else {
        json!({})
    };
    let root = settings
        .as_object_mut()
        .context("Codex hooks.json must be a JSON object")?;
    let hooks = object_field(root, "hooks", "Codex hooks key `hooks`")?;
    let session_start = array_field(hooks, "SessionStart", "Codex SessionStart hooks")?;

    let executable = std::env::current_exe().context("locate workspace-agent executable")?;
    let slot_default = shell_slot_default(slot)?;
    let command = managed_hook_command(format!(
        "{} report-codex --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&state_dir.to_string_lossy()),
        slot_default
    ));
    let updated = replace_workspace_agent_hook(session_start, "report-codex", &command);
    if !updated {
        session_start.push(json!({
            "matcher": "startup|resume|clear|compact",
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": 3,
                "statusMessage": "Saving Bastion session"
            }]
        }));
    }

    install_codex_state_hook(
        hooks,
        "UserPromptSubmit",
        "working",
        None,
        &executable,
        &state_dir,
        slot_default,
    )?;
    install_codex_state_hook(
        hooks,
        "PermissionRequest",
        "attention",
        Some("permission"),
        &executable,
        &state_dir,
        slot_default,
    )?;
    install_codex_state_hook(
        hooks,
        "Stop",
        "done",
        None,
        &executable,
        &state_dir,
        slot_default,
    )?;
    install_codex_state_hook(
        hooks,
        "Interrupt",
        "idle",
        None,
        &executable,
        &state_dir,
        slot_default,
    )?;
    install_codex_state_hook(
        hooks,
        "SessionEnd",
        "idle",
        None,
        &executable,
        &state_dir,
        slot_default,
    )?;

    if hooks_path.exists() {
        let backup = hooks_path.with_file_name("hooks.json.bastion-backup");
        fs::copy(&hooks_path, &backup)
            .with_context(|| format!("back up {}", hooks_path.display()))?;
        println!("Backed up existing hooks to {}", backup.display());
    }
    fs::write(&hooks_path, serde_json::to_string_pretty(&settings)? + "\n")
        .with_context(|| format!("write {}", hooks_path.display()))?;
    let action = if updated { "Updated" } else { "Installed" };
    println!(
        "{action} Codex SessionStart integration in {}",
        hooks_path.display()
    );
    println!("Open `/hooks` in Codex once and trust the Bastion hook.");
    Ok(())
}

fn report_codex(state_dir: PathBuf, slot: &str) -> Result<()> {
    report_json_session(state_dir, "codex", slot)
}

fn install_codex_state_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    state: &str,
    reason: Option<&str>,
    executable: &std::path::Path,
    state_dir: &std::path::Path,
    slot_default: &str,
) -> Result<()> {
    let groups = array_field(hooks, event, &format!("Codex {event} hooks"))?;
    let reason = reason
        .map(|value| format!(" --reason {}", shell_quote(value)))
        .unwrap_or_default();
    let command = managed_hook_command(format!(
        "{} report-codex-state --state {} --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"{}",
        shell_quote(&executable.to_string_lossy()),
        state,
        shell_quote(&state_dir.to_string_lossy()),
        slot_default,
        reason,
    ));
    let reporter = format!("report-codex-state --state {state}");
    if !replace_workspace_agent_hook(groups, &reporter, &command) {
        groups.push(json!({
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": 3,
                "statusMessage": "Updating Bastion pane state"
            }]
        }));
    }
    Ok(())
}

fn report_json_session(state_dir: PathBuf, agent: &str, slot: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        return Ok(());
    };
    let Some(session_id) = payload.get("session_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    report_session(state_dir, agent, slot, session_id, cwd)
}

fn install_antigravity(config_dir: PathBuf, state_dir: PathBuf, slot: &str) -> Result<()> {
    fs::create_dir_all(&config_dir).with_context(|| {
        format!(
            "create Antigravity config directory {}",
            config_dir.display()
        )
    })?;
    let path = config_dir.join("hooks.json");
    let mut root = if path.exists() {
        serde_json::from_str::<Value>(&fs::read_to_string(&path)?)?
    } else {
        json!({})
    };
    let object = root
        .as_object_mut()
        .context("Antigravity hooks.json must be a JSON object")?;
    let executable = std::env::current_exe().context("locate workspace-agent executable")?;
    let slot_default = shell_slot_default(slot)?;
    let working_command = managed_antigravity_hook_command(
        format!(
            "{} report-antigravity --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"",
            shell_quote(&executable.to_string_lossy()),
            shell_quote(&state_dir.to_string_lossy()),
            slot_default
        ),
        "{}",
    );
    let attention_command = managed_antigravity_hook_command(
        format!(
            "{} report-antigravity-attention --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"",
            shell_quote(&executable.to_string_lossy()),
            shell_quote(&state_dir.to_string_lossy()),
            slot_default
        ),
        r#"{"decision":"allow"}"#,
    );
    let stop_command = managed_antigravity_hook_command(
        format!(
            "{} report-antigravity-stop --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"",
            shell_quote(&executable.to_string_lossy()),
            shell_quote(&state_dir.to_string_lossy()),
            slot_default
        ),
        r#"{"decision":"allow"}"#,
    );
    object.insert(
        "bastion".to_owned(),
        json!({
            "PreInvocation": [{
                "type": "command",
                "command": working_command,
                "timeout": 3
            }],
            "PreToolUse": [{
                "matcher": "ask_permission",
                "hooks": [{
                    "type": "command",
                    "command": attention_command,
                    "timeout": 3
                }]
            }],
            "Stop": [{
                "type": "command",
                "command": stop_command,
                "timeout": 3
            }]
        }),
    );
    if path.exists() {
        fs::copy(&path, path.with_file_name("hooks.json.bastion-backup"))?;
    }
    fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")?;
    println!(
        "Enabled Antigravity session restore and lifecycle integration in {}",
        path.display()
    );
    Ok(())
}

fn uninstall_json_hooks(path: PathBuf, markers: &[&str], label: &str) -> Result<()> {
    if !path.exists() {
        println!("{label} integration: not installed");
        return Ok(());
    }
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parse {} as JSON", path.display()))?;
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        println!("{label} integration: not installed");
        return Ok(());
    };
    let mut removed = 0_usize;
    let events = hooks.keys().cloned().collect::<Vec<_>>();
    for event in events {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        for group in groups.iter_mut() {
            let Some(commands) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            let before = commands.len();
            commands.retain(|hook| {
                let command = hook.get("command").and_then(Value::as_str).unwrap_or("");
                !(command.contains("workspace-agent")
                    && markers.iter().any(|marker| command.contains(marker)))
            });
            removed += before.saturating_sub(commands.len());
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|commands| !commands.is_empty())
        });
        if groups.is_empty() {
            hooks.remove(&event);
        }
    }
    if removed == 0 {
        println!("{label} integration: not installed");
        return Ok(());
    }
    let backup = path.with_file_name(format!(
        "{}.bastion-uninstall-backup",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::copy(&path, &backup).with_context(|| format!("back up {}", path.display()))?;
    fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")
        .with_context(|| format!("write {}", path.display()))?;
    println!("Removed {label} integration ({removed} managed hook(s)).");
    Ok(())
}

fn uninstall_antigravity(path: PathBuf) -> Result<()> {
    if !path.exists() {
        println!("Antigravity integration: not installed");
        return Ok(());
    }
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parse {} as JSON", path.display()))?;
    let Some(root) = settings.as_object_mut() else {
        anyhow::bail!("Antigravity hooks.json must be a JSON object");
    };
    if root.remove("bastion").is_none() {
        println!("Antigravity integration: not installed");
        return Ok(());
    }
    let backup = path.with_file_name("hooks.json.bastion-uninstall-backup");
    fs::copy(&path, &backup).with_context(|| format!("back up {}", path.display()))?;
    fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")
        .with_context(|| format!("write {}", path.display()))?;
    println!("Removed Antigravity integration.");
    Ok(())
}

#[derive(Clone, Copy)]
enum AntigravityHook {
    PreInvocation,
    Attention,
    Stop,
}

fn report_antigravity(state_dir: PathBuf, slot: &str, hook: AntigravityHook) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        print_antigravity_response(hook);
        return Ok(());
    };
    let cwd = payload
        .get("workspacePaths")
        .and_then(Value::as_array)
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if let Some(session_id) = payload.get("conversationId").and_then(Value::as_str) {
        let _ = report_session(state_dir.clone(), "antigravity", slot, session_id, cwd);
    }
    match hook {
        AntigravityHook::PreInvocation => {
            let _ = report_agent_state(
                state_dir,
                "antigravity",
                slot,
                LifecycleState::Working,
                None,
            );
        }
        AntigravityHook::Attention => {
            let _ = report_agent_state(
                state_dir,
                "antigravity",
                slot,
                LifecycleState::Attention,
                Some("permission"),
            );
        }
        AntigravityHook::Stop => {
            let state = if payload
                .get("fullyIdle")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                LifecycleState::Done
            } else {
                LifecycleState::Working
            };
            let _ = report_agent_state(state_dir, "antigravity", slot, state, None);
        }
    }
    print_antigravity_response(hook);
    Ok(())
}

fn print_antigravity_response(hook: AntigravityHook) {
    match hook {
        AntigravityHook::PreInvocation => println!("{{}}"),
        // `allow` lets ask_permission display its own prompt; it does not grant
        // the resource permission being requested. Any non-continue decision
        // lets a Stop event finish normally.
        AntigravityHook::Attention | AntigravityHook::Stop => {
            println!(r#"{{"decision":"allow"}}"#);
        }
    }
}

fn report_session(
    state_dir: PathBuf,
    agent: &str,
    slot: &str,
    session_id: &str,
    cwd: PathBuf,
) -> Result<()> {
    let Ok(mut stream) = UnixStream::connect(state_dir.join("workspace.sock")) else {
        return Ok(());
    };
    let request = json!({
        "type": "report_agent_session",
        "agent": agent,
        "slot": slot,
        "session_id": session_id,
        "cwd": cwd,
    });
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    // The daemon acknowledges the update. Consume it before this short-lived
    // hook exits so the daemon does not write into a closed Unix socket.
    let mut acknowledgement = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        acknowledgement.push(byte[0]);
        if acknowledgement.len() > 64 * 1024 {
            break;
        }
    }
    Ok(())
}

fn report_agent_state(
    state_dir: PathBuf,
    agent: &str,
    slot: &str,
    state: LifecycleState,
    reason: Option<&str>,
) -> Result<()> {
    let Ok(mut stream) = UnixStream::connect(state_dir.join("workspace.sock")) else {
        return Ok(());
    };
    let request = json!({
        "type": "report_agent_state",
        "agent": agent,
        "slot": slot,
        "state": state.wire_name(),
        "reason": reason,
    });
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut acknowledgement = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' || acknowledgement.len() > 64 * 1024 {
            break;
        }
        acknowledgement.push(byte[0]);
    }
    Ok(())
}

fn object_field<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    description: &str,
) -> Result<&'a mut Map<String, Value>> {
    let value = parent.entry(key.to_owned()).or_insert_with(|| json!({}));
    value.as_object_mut().context(description.to_owned())
}

fn array_field<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    description: &str,
) -> Result<&'a mut Vec<Value>> {
    let value = parent.entry(key.to_owned()).or_insert_with(|| json!([]));
    value.as_array_mut().context(description.to_owned())
}

fn replace_workspace_agent_hook(groups: &mut [Value], reporter: &str, replacement: &str) -> bool {
    let mut updated = false;
    for group in groups {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for hook in hooks {
            let Some(command) = hook.get("command").and_then(Value::as_str) else {
                continue;
            };
            if command.contains("workspace-agent") && command.contains(reporter) {
                hook["command"] = Value::String(replacement.to_owned());
                updated = true;
            }
        }
    }
    updated
}

fn remove_workspace_agent_hooks(hooks: &mut Map<String, Value>, event: &str, reporter: &str) {
    let remove_event = if let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) {
        for group in groups.iter_mut() {
            if let Some(commands) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                commands.retain(|hook| {
                    let command = hook.get("command").and_then(Value::as_str).unwrap_or("");
                    !(command.contains("workspace-agent") && command.contains(reporter))
                });
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|commands| !commands.is_empty())
        });
        groups.is_empty()
    } else {
        false
    };
    if remove_event {
        hooks.remove(event);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\\"'\\\"'"))
}

/// Agent hooks are global configuration, while Bastion tracking belongs only
/// to panes it created. The daemon supplies this marker to every managed PTY.
fn managed_hook_command(command: String) -> String {
    format!("if [ \"${{BASTION_MANAGED_PANE:-}}\" = 1 ]; then {command}; fi")
}

/// Antigravity validates hook stdout even when the global hook is invoked by
/// an ordinary, non-Bastion session. Return a neutral response in that case.
fn managed_antigravity_hook_command(command: String, neutral_response: &str) -> String {
    format!(
        "if [ \"${{BASTION_MANAGED_PANE:-}}\" = 1 ]; then {command}; else printf '%s\\n' {}; fi",
        shell_quote(neutral_response)
    )
}

fn shell_slot_default(slot: &str) -> Result<&str> {
    if !slot.is_empty()
        && slot
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        Ok(slot)
    } else {
        anyhow::bail!(
            "slot names for hook integrations may contain only letters, numbers, '_' or '-'"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("bastion-agent-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn uninstall_removes_only_bastion_managed_hooks() {
        let root = test_dir("uninstall");
        let path = root.join("settings.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "SessionStart": [{"hooks": [
                        {"type": "command", "command": "/usr/bin/workspace-agent report-claude"},
                        {"type": "command", "command": "keep-my-hook"}
                    ]}],
                    "Stop": [{"hooks": [
                        {"type": "command", "command": "/usr/bin/workspace-agent report-claude-state --state done"}
                    ]}]
                },
                "theme": "user-setting"
            }))
            .unwrap(),
        )
        .unwrap();

        uninstall_json_hooks(path.clone(), &["report-claude"], "Claude").unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let rendered = value.to_string();
        assert!(!rendered.contains("workspace-agent"));
        assert!(rendered.contains("keep-my-hook"));
        assert_eq!(value["theme"], "user-setting");
        assert!(
            root.join("settings.json.bastion-uninstall-backup")
                .is_file()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_uninstall_preserves_other_hook_groups() {
        let root = test_dir("antigravity");
        let path = root.join("hooks.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "bastion": {"PreInvocation": []},
                "custom": {"PreInvocation": [{"command": "keep"}]}
            }))
            .unwrap(),
        )
        .unwrap();
        uninstall_antigravity(path.clone()).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("bastion").is_none());
        assert_eq!(value["custom"]["PreInvocation"][0]["command"], "keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_install_adds_supported_lifecycle_hooks() {
        let root = test_dir("antigravity-install");
        let config = root.join("config");
        let state = root.join("state");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("hooks.json"),
            serde_json::to_string_pretty(&json!({
                "custom": {"PostInvocation": [{"command": "keep"}]}
            }))
            .unwrap(),
        )
        .unwrap();

        install_antigravity(config.clone(), state, "primary").unwrap();
        let value: Value =
            serde_json::from_str(&fs::read_to_string(config.join("hooks.json")).unwrap()).unwrap();
        let bastion = &value["bastion"];
        assert_eq!(bastion["PreToolUse"][0]["matcher"], "ask_permission");
        assert!(
            bastion["PreInvocation"][0]["command"]
                .as_str()
                .unwrap()
                .contains("report-antigravity")
        );
        assert!(
            bastion["PreToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("report-antigravity-attention")
        );
        assert!(
            bastion["Stop"][0]["command"]
                .as_str()
                .unwrap()
                .contains("report-antigravity-stop")
        );
        assert_eq!(value["custom"]["PostInvocation"][0]["command"], "keep");
        assert!(config.join("hooks.json.bastion-backup").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
