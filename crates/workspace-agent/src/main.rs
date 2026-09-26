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
        Command::ReportAntigravity { state_dir, slot } => report_antigravity(state_dir, &slot),
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
    let command = managed_hook_command(format!(
        "{} report-antigravity --state-dir {} --slot \"${{WORKSPACE_AGENT_SLOT:-{}}}\"",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&state_dir.to_string_lossy()),
        slot_default
    ));
    object.insert(
        "bastion".to_owned(),
        json!({"PreInvocation": [{"type":"command", "command": command, "timeout": 10}]}),
    );
    if path.exists() {
        fs::copy(&path, path.with_file_name("hooks.json.bastion-backup"))?;
    }
    fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")?;
    println!("Enabled Antigravity session restore in {}", path.display());
    Ok(())
}

fn report_antigravity(state_dir: PathBuf, slot: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        return Ok(());
    };
    let Some(session_id) = payload.get("conversationId").and_then(Value::as_str) else {
        return Ok(());
    };
    let cwd = payload
        .get("workspacePaths")
        .and_then(Value::as_array)
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    report_session(state_dir, "antigravity", slot, session_id, cwd)
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
