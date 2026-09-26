mod agents;

use anyhow::{Context, Result};
use clap::Parser;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime},
};
use uuid::Uuid;
use workspace_core::StateDb;
use workspace_protocol::{AgentNotification, AgentState, DAEMON_REVISION, Request};
use workspace_terminal::Terminal;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_os_t = default_state_dir())]
    state_dir: PathBuf,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/termux-agent-workspace")
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    Error {
        message: String,
    },
    Status {
        daemon_revision: u32,
        panes: Vec<String>,
        pane_details: Vec<PaneSummary>,
        recent: Vec<RecentPane>,
    },
    Started {
        pane_id: String,
    },
    Slots {
        slots: Vec<workspace_core::AgentSlot>,
    },
    Tabs {
        tabs: Vec<workspace_core::WorkspaceTab>,
    },
    Workspaces {
        workspaces: Vec<WorkspaceSummary>,
    },
    TabCreated {
        tab: workspace_core::WorkspaceTab,
    },
    Deleted {
        message: String,
    },
    Recorded {
        slot: workspace_core::AgentSlot,
    },
    Attached {
        pane_id: String,
    },
    PaneLog {
        pane_id: String,
        exit_code: Option<u32>,
        output: String,
    },
    PaneSnapshot {
        pane_id: String,
        rows: u16,
        cols: u16,
        output: String,
        cells: Vec<Vec<SnapshotCell>>,
    },
    Notifications {
        latest_sequence: u64,
        notifications: Vec<AgentNotification>,
    },
}

/// A serialized terminal cell.  Sending cells rather than re-playing an ANSI
/// dump preserves cursor movement, cleared regions, and popup overlays.
#[derive(Serialize)]
struct SnapshotCell {
    text: String,
    wide_continuation: bool,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    foreground: workspace_terminal::CellColor,
    background: workspace_terminal::CellColor,
}

struct Pane {
    project: workspace_core::Project,
    writer: Mutex<Box<dyn Write + Send>>,
    clients: Mutex<Vec<mpsc::Sender<Vec<u8>>>>,
    history: Mutex<VecDeque<u8>>,
    last_output: Mutex<Instant>,
    screen: Mutex<Terminal>,
    _master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    command: String,
    label: Mutex<String>,
    agent_slots: Mutex<Vec<(String, String)>>,
    /// Opaque slot inherited by a normal shell. Claude's hook and the Codex
    /// watcher use it to persist a session the user started manually.
    tracking_slot: Option<String>,
    tab: Mutex<String>,
    agent_state: Mutex<AgentState>,
}

#[derive(Clone, Serialize)]
struct PaneSummary {
    pane_id: String,
    workspace_root: String,
    tab: String,
    command: String,
    label: String,
    agent_kind: Option<String>,
    resume_command: Option<String>,
    health: &'static str,
    idle_seconds: u64,
    agent_state: AgentState,
}

#[derive(Serialize)]
struct WorkspaceSummary {
    id: i64,
    canonical_root: String,
    pane_count: i64,
    agent_summary: Option<String>,
}

#[derive(Clone, Serialize)]
struct RecentPane {
    pane_id: String,
    command: String,
    exit_code: Option<u32>,
    output_bytes: usize,
    #[serde(skip_serializing)]
    output: String,
}

struct Daemon {
    database: Mutex<StateDb>,
    default_cwd: PathBuf,
    panes: Mutex<HashMap<String, Arc<Pane>>>,
    recent: Mutex<VecDeque<RecentPane>>,
    notifications: Mutex<VecDeque<AgentNotification>>,
    next_notification_sequence: Mutex<u64>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let state_dir = args
        .state_dir
        .canonicalize()
        .unwrap_or(args.state_dir.clone());
    std::fs::create_dir_all(&state_dir)?;
    let socket = state_dir.join("workspace.sock");
    if socket.exists() {
        match UnixStream::connect(&socket) {
            Ok(_) => anyhow::bail!(
                "workspace-daemon is already running for {}",
                state_dir.display()
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(&socket)?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("check existing workspace socket {}", socket.display())
                });
            }
        }
    }
    let database = StateDb::open(&state_dir)?;
    let _pid_file = DaemonPidFile::create(state_dir.join("workspace-daemon.pid"))?;
    database.mark_all_panes_stopped()?;
    let daemon = Arc::new(Daemon {
        database: Mutex::new(database),
        default_cwd: args.cwd.canonicalize().context("resolve daemon cwd")?,
        panes: Mutex::new(HashMap::new()),
        recent: Mutex::new(VecDeque::new()),
        notifications: Mutex::new(VecDeque::new()),
        next_notification_sequence: Mutex::new(1),
    });
    if let Err(error) = restore_saved_agents(&daemon) {
        eprintln!("agent restore skipped: {error:#}");
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    eprintln!("workspace-daemon listening on {}", socket.display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                thread::spawn(move || {
                    let response_stream = stream.try_clone();
                    if let Err(error) = handle_client(daemon, stream) {
                        if let Ok(mut response_stream) = response_stream {
                            let _ = write_response(
                                &mut response_stream,
                                Response::Error {
                                    message: format!("{error:#}"),
                                },
                            );
                        }
                        eprintln!("client error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("accept error: {error}"),
        }
    }
    Ok(())
}

fn restore_saved_agents(daemon: &Arc<Daemon>) -> Result<()> {
    let projects = daemon.database.lock().unwrap().list_projects()?;
    for project in projects {
        let cwd = PathBuf::from(&project.canonical_root);
        let slots = daemon.database.lock().unwrap().restorable_slots(&project)?;
        for slot in slots {
            let tab = slot.last_tab.as_deref().unwrap_or("Main");
            match resume_agent(
                daemon,
                &slot.agent_kind,
                &slot.slot_name,
                cwd.clone(),
                Some(tab),
            ) {
                Ok(pane_id) => eprintln!(
                    "restored {}/{} in {} as {pane_id}",
                    slot.agent_kind, slot.slot_name, project.canonical_root
                ),
                Err(error) => eprintln!(
                    "could not restore {}/{} in {}: {error:#}",
                    slot.agent_kind, slot.slot_name, project.canonical_root
                ),
            }
        }
    }
    Ok(())
}

fn handle_client(daemon: Arc<Daemon>, mut stream: UnixStream) -> Result<()> {
    // A connect-only probe (used to see whether the daemon already owns its
    // socket) is not a malformed client request.  Let it close quietly.
    let Some(line) = read_line(&mut stream)? else {
        return Ok(());
    };
    let request: Request = serde_json::from_slice(&line)?;
    match request {
        Request::Status => {
            let active = daemon.panes.lock().unwrap();
            let panes = active.keys().cloned().collect();
            let pane_details = active
                .iter()
                .map(|(pane_id, pane)| PaneSummary {
                    pane_id: pane_id.clone(),
                    workspace_root: pane.project.canonical_root.clone(),
                    tab: pane.tab.lock().unwrap().clone(),
                    command: pane.command.clone(),
                    label: pane.label.lock().unwrap().clone(),
                    agent_kind: pane_agent_kind(pane),
                    resume_command: pane_resume_command(&daemon, pane),
                    health: pane_health(pane),
                    idle_seconds: pane.last_output.lock().unwrap().elapsed().as_secs(),
                    agent_state: *pane.agent_state.lock().unwrap(),
                })
                .collect();
            let recent = daemon.recent.lock().unwrap().iter().cloned().collect();
            write_response(
                &mut stream,
                Response::Status {
                    daemon_revision: DAEMON_REVISION,
                    panes,
                    pane_details,
                    recent,
                },
            )
        }
        Request::ListWorkspaces => {
            let live = daemon.panes.lock().unwrap().values().fold(
                HashMap::<i64, (i64, Option<String>, &'static str)>::new(),
                |mut map, pane| {
                    let entry = map.entry(pane.project.id).or_insert((
                        0,
                        pane_agent_kind(pane),
                        pane_health(pane),
                    ));
                    entry.0 += 1;
                    if entry.1.is_none() {
                        entry.1 = pane_agent_kind(pane);
                    }
                    if pane_health(pane) == "active" {
                        entry.2 = "active";
                    }
                    map
                },
            );
            let database = daemon.database.lock().unwrap();
            let workspaces = database
                .list_projects()?
                .into_iter()
                .map(|project| {
                    let live_state = live.get(&project.id);
                    let pane_count = live_state.map(|state| state.0).unwrap_or(0);
                    let saved_summary = database
                        .list_slots(&project)?
                        .into_iter()
                        .find(|slot| slot.restore_enabled && slot.last_state != "stopped")
                        .map(|slot| format!("{} saved", slot.agent_kind));
                    let agent_summary = live_state
                        .map(|(_, agent, health)| {
                            format!("{} {health}", agent.as_deref().unwrap_or("shell"))
                        })
                        .or(saved_summary);
                    Ok(WorkspaceSummary {
                        id: project.id,
                        canonical_root: project.canonical_root,
                        pane_count,
                        agent_summary,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            write_response(&mut stream, Response::Workspaces { workspaces })
        }
        Request::FocusWorkspace { cwd } => {
            let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
            daemon.database.lock().unwrap().focus_project(&project)?;
            write_response(
                &mut stream,
                Response::Deleted {
                    message: format!("focused {}", project.canonical_root),
                },
            )
        }
        Request::RemoveWorkspace { cwd, force } => {
            remove_workspace(&daemon, cwd, force, &mut stream)
        }
        Request::StartShell { cwd, tab } => {
            let pane_id = start_shell(
                &daemon,
                cwd.unwrap_or_else(|| daemon.default_cwd.clone()),
                tab.as_deref(),
            )?;
            write_response(&mut stream, Response::Started { pane_id })
        }
        Request::StartAgent {
            agent,
            slot,
            cwd,
            tab,
        } => {
            let pane_id = start_agent(
                &daemon,
                &agent,
                &slot,
                cwd.unwrap_or_else(|| daemon.default_cwd.clone()),
                tab.as_deref(),
            )?;
            write_response(&mut stream, Response::Started { pane_id })
        }
        Request::ResumeAgent {
            agent,
            slot,
            cwd,
            tab,
        } => {
            let pane_id = resume_agent(
                &daemon,
                &agent,
                &slot,
                cwd.unwrap_or_else(|| daemon.default_cwd.clone()),
                tab.as_deref(),
            )?;
            write_response(&mut stream, Response::Started { pane_id })
        }
        Request::ReportAgentSession {
            agent,
            slot,
            session_id,
            cwd,
        } => {
            // Hooks can report a directory after the agent has changed it, or
            // with a directory normalized differently from its launching
            // shell. A managed pane's UUID tracking slot is authoritative:
            // it prevents an otherwise valid native session from being saved
            // under a different workspace and restored in the wrong place.
            let owner = daemon
                .panes
                .lock()
                .unwrap()
                .values()
                .find(|pane| pane.tracking_slot.as_deref() == Some(slot.as_str()))
                .cloned();
            let project = match &owner {
                Some(pane) => pane.project.clone(),
                None => {
                    let cwd = cwd.unwrap_or_else(|| daemon.default_cwd.clone());
                    daemon.database.lock().unwrap().ensure_project(&cwd)?
                }
            };
            let agent_slot = daemon.database.lock().unwrap().record_agent_session(
                &project,
                &agent,
                &slot,
                &session_id,
            )?;
            // A normal +Pane shell carries a unique tracking slot. Link this
            // hook report back to it so stopping that pane also forgets the
            // resumable session.
            if let Some(pane) = owner {
                let tab = pane.tab.lock().unwrap().clone();
                daemon
                    .database
                    .lock()
                    .unwrap()
                    .set_slot_tab(&project, &agent, &slot, &tab)?;
                let mut tracked = pane.agent_slots.lock().unwrap();
                if !tracked
                    .iter()
                    .any(|item| item == &(agent.clone(), slot.clone()))
                {
                    tracked.push((agent.clone(), slot.clone()));
                }
            }
            write_response(&mut stream, Response::Recorded { slot: agent_slot })
        }
        Request::ReportAgentState {
            agent,
            slot,
            state,
            reason,
        } => {
            let owner = daemon
                .panes
                .lock()
                .unwrap()
                .iter()
                .find(|(_, pane)| pane.tracking_slot.as_deref() == Some(slot.as_str()))
                .map(|(pane_id, pane)| (pane_id.clone(), Arc::clone(pane)));
            let Some((pane_id, pane)) = owner else {
                return write_response(
                    &mut stream,
                    Response::Error {
                        message: format!("managed pane for slot {slot} was not found"),
                    },
                );
            };
            let previous = {
                let mut current = pane.agent_state.lock().unwrap();
                let previous = *current;
                *current = state;
                previous
            };
            let should_notify = previous != state
                && match state {
                    AgentState::Attention => previous == AgentState::Working,
                    AgentState::Done => {
                        matches!(previous, AgentState::Working | AgentState::Attention)
                    }
                    _ => false,
                };
            if should_notify {
                let sequence = {
                    let mut next = daemon.next_notification_sequence.lock().unwrap();
                    let sequence = *next;
                    *next = next.saturating_add(1);
                    sequence
                };
                let mut notifications = daemon.notifications.lock().unwrap();
                notifications.push_back(AgentNotification {
                    sequence,
                    pane_id,
                    agent,
                    state,
                    reason,
                });
                while notifications.len() > 128 {
                    notifications.pop_front();
                }
            }
            write_response(
                &mut stream,
                Response::Deleted {
                    message: format!("agent state recorded: {state:?}"),
                },
            )
        }
        Request::ListNotifications { after_sequence } => {
            let latest_sequence =
                (*daemon.next_notification_sequence.lock().unwrap()).saturating_sub(1);
            let notifications = daemon.notifications.lock().unwrap();
            let notifications = notifications
                .iter()
                .filter(|item| item.sequence > after_sequence)
                .cloned()
                .collect();
            write_response(
                &mut stream,
                Response::Notifications {
                    latest_sequence,
                    notifications,
                },
            )
        }
        Request::ListSlots { cwd } => {
            let cwd = cwd.unwrap_or_else(|| daemon.default_cwd.clone());
            let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
            let slots = daemon.database.lock().unwrap().list_slots(&project)?;
            write_response(&mut stream, Response::Slots { slots })
        }
        Request::ListTabs { cwd } => {
            let cwd = cwd.unwrap_or_else(|| daemon.default_cwd.clone());
            let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
            let tabs = daemon.database.lock().unwrap().list_tabs(&project)?;
            write_response(&mut stream, Response::Tabs { tabs })
        }
        Request::CreateTab { name, cwd } => {
            let cwd = cwd.unwrap_or_else(|| daemon.default_cwd.clone());
            let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
            let tab = daemon
                .database
                .lock()
                .unwrap()
                .ensure_tab(&project, &name)?;
            write_response(&mut stream, Response::TabCreated { tab })
        }
        Request::DeleteTab { name, cwd } => {
            if daemon
                .panes
                .lock()
                .unwrap()
                .values()
                .any(|pane| *pane.tab.lock().unwrap() == name)
            {
                return write_response(
                    &mut stream,
                    Response::Error {
                        message: format!("tab {name} still has active panes"),
                    },
                );
            }
            let cwd = cwd.unwrap_or_else(|| daemon.default_cwd.clone());
            let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
            daemon
                .database
                .lock()
                .unwrap()
                .delete_tab(&project, &name)?;
            write_response(
                &mut stream,
                Response::Deleted {
                    message: format!("tab {name} deleted"),
                },
            )
        }
        Request::PaneLog { pane_id } => pane_log(&daemon, pane_id, &mut stream),
        Request::PaneSnapshot { pane_id } => pane_snapshot(&daemon, pane_id, &mut stream),
        Request::StopPane { pane_id } => stop_pane(&daemon, pane_id, &mut stream),
        Request::RenamePane { pane_id, name } => rename_pane(&daemon, pane_id, name, &mut stream),
        Request::MovePane { pane_id, tab, cwd } => move_pane(
            &daemon,
            pane_id,
            tab,
            cwd.unwrap_or_else(|| daemon.default_cwd.clone()),
            &mut stream,
        ),
        Request::Attach {
            pane_id,
            cols,
            rows,
        } => attach(&daemon, pane_id, cols, rows, stream),
    }
}

/// A short-lived ownership record for the launcher. It is cleaned up on a
/// normal exit; after a crash, the next daemon simply overwrites the stale
/// record once it has acquired the socket.
struct DaemonPidFile {
    path: PathBuf,
    pid: u32,
}

impl DaemonPidFile {
    fn create(path: PathBuf) -> Result<Self> {
        std::fs::write(&path, std::process::id().to_string())
            .with_context(|| format!("write daemon PID {}", path.display()))?;
        Ok(Self {
            path,
            pid: std::process::id(),
        })
    }
}

impl Drop for DaemonPidFile {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path)
            .ok()
            .is_some_and(|value| value.trim() == self.pid.to_string())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn remove_workspace(
    daemon: &Arc<Daemon>,
    cwd: PathBuf,
    force: bool,
    stream: &mut UnixStream,
) -> Result<()> {
    let project = daemon
        .database
        .lock()
        .unwrap()
        .project_at(&cwd)?
        .context("workspace is not registered")?;
    let active = daemon
        .panes
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, pane)| pane.project.id == project.id)
        .map(|(id, pane)| (id.clone(), Arc::clone(pane)))
        .collect::<Vec<_>>();
    if !active.is_empty() && !force {
        let labels = active
            .iter()
            .map(|(_, pane)| pane.label.lock().unwrap().clone())
            .collect::<Vec<_>>()
            .join(", ");
        return write_response(
            stream,
            Response::Error {
                message: format!(
                    "workspace has {} running pane(s): {labels}; retry with --force to stop and remove them",
                    active.len()
                ),
            },
        );
    }
    for (_, pane) in &active {
        // The daemon owns these PTYs. Killing them is required before their
        // state can be forgotten; project files are never touched.
        let _ = pane.child.lock().unwrap().kill();
    }
    daemon.database.lock().unwrap().delete_project(&project)?;
    write_response(
        stream,
        Response::Deleted {
            message: format!("removed workspace state for {}", project.canonical_root),
        },
    )
}

fn stop_pane(daemon: &Arc<Daemon>, pane_id: String, stream: &mut UnixStream) -> Result<()> {
    let Some(pane) = daemon.panes.lock().unwrap().get(&pane_id).cloned() else {
        return write_response(
            stream,
            Response::Error {
                message: format!("pane not found: {pane_id}"),
            },
        );
    };
    forget_pane_agent_slots(daemon, &pane)?;
    pane.child.lock().unwrap().kill()?;
    write_response(
        stream,
        Response::Deleted {
            message: format!("stopping pane {pane_id}"),
        },
    )
}

fn forget_pane_agent_slots(daemon: &Arc<Daemon>, pane: &Pane) -> Result<()> {
    let slots = pane.agent_slots.lock().unwrap().clone();
    if slots.is_empty() {
        return Ok(());
    }
    for (agent, slot) in slots {
        daemon
            .database
            .lock()
            .unwrap()
            .forget_slot(&pane.project, &agent, &slot)?;
    }
    Ok(())
}

fn rename_pane(
    daemon: &Arc<Daemon>,
    pane_id: String,
    name: String,
    stream: &mut UnixStream,
) -> Result<()> {
    let Some(pane) = daemon.panes.lock().unwrap().get(&pane_id).cloned() else {
        return write_response(
            stream,
            Response::Error {
                message: format!("pane not found: {pane_id}"),
            },
        );
    };
    let name = name.trim();
    daemon
        .database
        .lock()
        .unwrap()
        .rename_pane(&pane_id, name)?;
    *pane.label.lock().unwrap() = name.to_owned();
    write_response(
        stream,
        Response::Deleted {
            message: format!("renamed pane to {name}"),
        },
    )
}

fn move_pane(
    daemon: &Arc<Daemon>,
    pane_id: String,
    tab_name: String,
    cwd: PathBuf,
    stream: &mut UnixStream,
) -> Result<()> {
    let Some(pane) = daemon.panes.lock().unwrap().get(&pane_id).cloned() else {
        return write_response(
            stream,
            Response::Error {
                message: format!("pane not found: {pane_id}"),
            },
        );
    };
    let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
    let tab = daemon
        .database
        .lock()
        .unwrap()
        .move_pane(&pane_id, &project, &tab_name)?;
    *pane.tab.lock().unwrap() = tab.name.clone();
    write_response(
        stream,
        Response::Deleted {
            message: format!("moved pane to {}", tab.name),
        },
    )
}

fn start_shell(daemon: &Arc<Daemon>, cwd: PathBuf, tab: Option<&str>) -> Result<String> {
    let tab_id = tab_for(daemon, &cwd, tab)?;
    let started_at = SystemTime::now();
    let tracking_slot = format!("pane-{}", Uuid::new_v4());
    let shell = preferred_shell();
    let pane_id = spawn_pane(
        daemon,
        cwd.clone(),
        &shell,
        &["-i"],
        None,
        Some(tracking_slot.clone()),
        tab_id,
    )?;
    // Adapter-specific observers may discover a session the user starts from
    // an ordinary shell. The generic pane manager does not inspect agents.
    watch_codex_session(Arc::clone(daemon), cwd, tracking_slot, started_at);
    Ok(pane_id)
}

/// Normal panes should feel like a regular Termux terminal.  In particular,
/// do not suppress the user's rc file: aliases, functions, PATH additions,
/// and prompt configuration are part of the shell they selected.
fn preferred_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "bash".to_owned())
}

fn start_agent(
    daemon: &Arc<Daemon>,
    agent: &str,
    slot: &str,
    cwd: PathBuf,
    tab: Option<&str>,
) -> Result<String> {
    let adapter = agent_adapter(agent)?;
    let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
    daemon
        .database
        .lock()
        .unwrap()
        .ensure_slot(&project, agent, slot)?;
    let started_at = SystemTime::now();
    let tab_id = tab_for(daemon, &cwd, tab)?;
    daemon
        .database
        .lock()
        .unwrap()
        .set_slot_tab(&project, agent, slot, &tab_id.name)?;
    let pane_id = spawn_pane(
        daemon,
        cwd.clone(),
        adapter.executable(),
        &[],
        Some(slot),
        Some(slot.to_owned()),
        tab_id,
    )?;
    if adapter.id() == "codex" {
        watch_codex_session(Arc::clone(daemon), cwd, slot.to_owned(), started_at);
    }
    Ok(pane_id)
}

fn resume_agent(
    daemon: &Arc<Daemon>,
    agent: &str,
    slot: &str,
    cwd: PathBuf,
    tab: Option<&str>,
) -> Result<String> {
    let adapter = agent_adapter(agent)?;
    let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
    let stored = daemon
        .database
        .lock()
        .unwrap()
        .slot(&project, agent, slot)?
        .context(format!("no {agent} session is stored for slot {slot}"))?;
    let session_id = stored
        .native_session_id
        .context(format!("slot {slot} has no native session ID yet"))?;
    let tab_id = tab_for(daemon, &cwd, tab)?;
    daemon
        .database
        .lock()
        .unwrap()
        .set_slot_tab(&project, agent, slot, &tab_id.name)?;
    let arguments = adapter.resume_arguments(&session_id);
    let argument_refs = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    spawn_pane(
        daemon,
        cwd,
        adapter.executable(),
        &argument_refs,
        Some(slot),
        Some(slot.to_owned()),
        tab_id,
    )
}

fn tab_for(
    daemon: &Arc<Daemon>,
    cwd: &std::path::Path,
    tab: Option<&str>,
) -> Result<workspace_core::WorkspaceTab> {
    let database = daemon.database.lock().unwrap();
    let project = database.ensure_project(cwd)?;
    let tab = database.ensure_tab(&project, tab.unwrap_or("Main"))?;
    Ok(tab)
}

fn agent_adapter(agent: &str) -> Result<&'static dyn agents::AgentAdapter> {
    agents::named(agent).with_context(|| {
        format!(
            "unknown agent adapter: {agent}. Generic programs belong in +Pane; add an adapter only when Bastion needs its native resume/session behavior"
        )
    })
}

fn watch_codex_session(daemon: Arc<Daemon>, cwd: PathBuf, slot: String, started_at: SystemTime) {
    thread::spawn(move || {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")));
        let Some(codex_home) = codex_home else {
            return;
        };
        let session_root = codex_home.join("sessions");
        // Codex creates its local rollout after the user submits its first
        // prompt, not necessarily when its TUI opens. Keep watching long
        // enough for a real interactive session without polling aggressively.
        for _ in 0..600 {
            if let Some(session_id) = newest_codex_session(&session_root, &cwd, started_at) {
                let result = (|| -> Result<()> {
                    let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
                    daemon.database.lock().unwrap().record_agent_session(
                        &project,
                        "codex",
                        &slot,
                        &session_id,
                    )?;
                    for pane in daemon.panes.lock().unwrap().values() {
                        if pane.tracking_slot.as_deref() == Some(slot.as_str()) {
                            let mut tracked = pane.agent_slots.lock().unwrap();
                            if !tracked
                                .iter()
                                .any(|item| item.0 == "codex" && item.1 == slot)
                            {
                                tracked.push(("codex".to_owned(), slot.clone()));
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("could not record Codex session: {error:#}");
                }
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
        eprintln!("Codex session watcher timed out after 10 minutes for slot {slot}");
    });
}

fn newest_codex_session(
    root: &std::path::Path,
    cwd: &std::path::Path,
    started_at: SystemTime,
) -> Option<String> {
    let mut candidates = Vec::new();
    collect_codex_rollouts(root, &mut candidates);
    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    });
    candidates.reverse();
    for path in candidates {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        // Filesystem timestamp resolution varies. Allow two seconds of leeway,
        // then require the metadata's workspace path to match exactly.
        if modified + Duration::from_secs(2) < started_at {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(line) = contents.lines().next() else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(payload) = metadata.get("payload") else {
            continue;
        };
        let Some(recorded_cwd) = payload.get("cwd").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if PathBuf::from(recorded_cwd).canonicalize().ok() != cwd.canonicalize().ok() {
            continue;
        }
        if let Some(session_id) = payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
        {
            return Some(session_id.to_owned());
        }
    }
    None
}

fn collect_codex_rollouts(root: &std::path::Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_codex_rollouts(&path, output);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("rollout-"))
        {
            output.push(path);
        }
    }
}

fn spawn_pane(
    daemon: &Arc<Daemon>,
    cwd: PathBuf,
    executable: &str,
    arguments: &[&str],
    agent_slot: Option<&str>,
    tracking_slot: Option<String>,
    tab: workspace_core::WorkspaceTab,
) -> Result<String> {
    let project = daemon.database.lock().unwrap().ensure_project(&cwd)?;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let pane_id = Uuid::new_v4().to_string();
    let mut command = CommandBuilder::new(executable);
    command.args(arguments);
    command.cwd(&cwd);
    command.env("TERM", "xterm-256color");
    // Global agent hooks should only report sessions started inside Bastion.
    // This prevents a separate, ordinary `codex`/`claude` invocation from
    // colliding with a managed pane's default slot.
    command.env("BASTION_MANAGED_PANE", "1");
    if let Some(slot) = tracking_slot.as_deref().or(agent_slot) {
        command.env("WORKSPACE_AGENT_SLOT", slot);
    }
    let child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let label = next_pane_label(daemon, &project);
    let pane = Arc::new(Pane {
        project: project.clone(),
        writer: Mutex::new(writer),
        clients: Mutex::new(Vec::new()),
        history: Mutex::new(VecDeque::new()),
        last_output: Mutex::new(Instant::now()),
        screen: Mutex::new(Terminal::new(40, 120)),
        _master: Mutex::new(pair.master),
        child: Mutex::new(child),
        command: format!("{executable} {}", arguments.join(" ")),
        label: Mutex::new(label.clone()),
        agent_slots: Mutex::new(
            agent_slot
                .map(|slot| vec![(executable.to_owned(), slot.to_owned())])
                .unwrap_or_default(),
        ),
        tracking_slot,
        tab: Mutex::new(tab.name.clone()),
        agent_state: Mutex::new(AgentState::Unknown),
    });
    daemon.database.lock().unwrap().record_pane(
        &pane_id,
        project.id,
        tab.id,
        &label,
        &format!("{executable} {}", arguments.join(" ")),
    )?;
    daemon
        .panes
        .lock()
        .unwrap()
        .insert(pane_id.clone(), Arc::clone(&pane));
    let daemon_for_pump = Arc::clone(daemon);
    let pane_id_for_pump = pane_id.clone();
    thread::spawn(move || pump_pty(reader, pane, daemon_for_pump, pane_id_for_pump));
    Ok(pane_id)
}

/// Use the lowest available human-facing pane number.  Pane labels are not
/// identities, so a deleted `Pane 1` makes that friendly name available for
/// the next pane instead of leaving surprising gaps.
fn next_pane_label(daemon: &Arc<Daemon>, project: &workspace_core::Project) -> String {
    let panes = daemon.panes.lock().unwrap();
    for number in 1_u64.. {
        let candidate = format!("Pane {number}");
        let taken = panes
            .values()
            .any(|pane| pane.project.id == project.id && *pane.label.lock().unwrap() == candidate);
        if !taken {
            return candidate;
        }
    }
    unreachable!("unbounded pane label search must find a free number")
}

fn pump_pty(
    mut reader: Box<dyn Read + Send>,
    pane: Arc<Pane>,
    daemon: Arc<Daemon>,
    pane_id: String,
) {
    let mut buffer = [0_u8; 4096];
    while let Ok(size) = reader.read(&mut buffer) {
        if size == 0 {
            break;
        }
        let data = buffer[..size].to_vec();
        *pane.last_output.lock().unwrap() = Instant::now();
        let replies = {
            let mut screen = pane.screen.lock().unwrap();
            screen.process(&data);
            screen.take_replies()
        };
        // An attached client is the active terminal and returns its own
        // capability replies. If nobody is attached, answer from the daemon
        // so a headless snapshot still behaves like a real terminal.
        if pane.clients.lock().unwrap().is_empty() {
            let mut writer = pane.writer.lock().unwrap();
            for reply in replies {
                let _ = writer.write_all(&reply);
            }
            let _ = writer.flush();
        }
        {
            const HISTORY_LIMIT: usize = 256 * 1024;
            let mut history = pane.history.lock().unwrap();
            history.extend(data.iter().copied());
            if history.len() > HISTORY_LIMIT {
                let overflow = history.len() - HISTORY_LIMIT;
                history.drain(..overflow);
            }
        }
        pane.clients
            .lock()
            .unwrap()
            .retain(|client| client.send(data.clone()).is_ok());
    }
    pane.clients.lock().unwrap().clear();
    let exit_code = pane
        .child
        .lock()
        .unwrap()
        .try_wait()
        .ok()
        .flatten()
        .map(|status| status.exit_code());
    let output_bytes = pane.history.lock().unwrap().len();
    let output = history_tail(&pane.history.lock().unwrap());
    let _ = daemon.database.lock().unwrap().mark_pane_stopped(&pane_id);
    // A process that exits has no pane left to restore.  This also removes a
    // bad native session ID (for example, one purged by Claude) so it cannot
    // keep producing a misleading "saved" workspace on every restart.
    let _ = forget_pane_agent_slots(&daemon, &pane);
    daemon.panes.lock().unwrap().remove(&pane_id);
    let mut recent = daemon.recent.lock().unwrap();
    recent.push_front(RecentPane {
        pane_id,
        command: pane.command.clone(),
        exit_code,
        output_bytes,
        output,
    });
    recent.truncate(20);
}

/// A factual process-level state. Agent-specific semantic states are layered
/// separately; no quiet PTY is incorrectly labelled as an agent failure.
fn pane_health(pane: &Pane) -> &'static str {
    if pane.last_output.lock().unwrap().elapsed() <= Duration::from_secs(8) {
        "active"
    } else {
        "waiting"
    }
}

fn pane_agent_kind(pane: &Pane) -> Option<String> {
    if let Some((agent, _)) = pane.agent_slots.lock().unwrap().first() {
        return Some(agent.clone());
    }
    let executable = pane
        .command
        .split_whitespace()
        .next()
        .and_then(agents::for_executable)
        .map(|adapter| adapter.id().to_owned());
    executable.or_else(|| {
        agents::detect_screen(&pane.screen.lock().unwrap().snapshot().output).map(str::to_owned)
    })
}

/// A resume command is shown to the user as a preview only. The daemon still
/// owns actual restore behavior and only restores slots with a real native ID.
fn pane_resume_command(daemon: &Daemon, pane: &Pane) -> Option<String> {
    let tracked = pane.agent_slots.lock().unwrap().clone();
    for (agent, slot) in tracked {
        let adapter = agents::named(&agent)?;
        let stored = daemon
            .database
            .lock()
            .unwrap()
            .slot(&pane.project, &agent, &slot)
            .ok()??;
        if stored.restore_enabled && stored.last_state != "stopped" {
            if let Some(session_id) = stored.native_session_id {
                return Some(adapter.resume_command(&session_id));
            }
        }
    }
    None
}

fn pane_snapshot(daemon: &Arc<Daemon>, pane_id: String, stream: &mut UnixStream) -> Result<()> {
    let active = daemon.panes.lock().unwrap().get(&pane_id).cloned();
    let Some(pane) = active else {
        return write_response(
            &mut *stream,
            Response::Error {
                message: format!("pane not found: {pane_id}"),
            },
        );
    };
    let snapshot = pane.screen.lock().unwrap().snapshot();
    let rows = snapshot.rows;
    let cols = snapshot.cols;
    let output = snapshot.output;
    let cells = snapshot
        .cells
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| SnapshotCell {
                    text: cell.text,
                    wide_continuation: cell.wide_continuation,
                    bold: cell.bold,
                    italic: cell.italic,
                    underline: cell.underline,
                    inverse: cell.inverse,
                    foreground: cell.foreground,
                    background: cell.background,
                })
                .collect()
        })
        .collect();
    write_response(
        &mut *stream,
        Response::PaneSnapshot {
            pane_id,
            rows,
            cols,
            output,
            cells,
        },
    )
}

fn pane_log(daemon: &Arc<Daemon>, pane_id: String, stream: &mut UnixStream) -> Result<()> {
    let active = daemon.panes.lock().unwrap().get(&pane_id).cloned();
    if let Some(pane) = active {
        let output = history_tail(&pane.history.lock().unwrap());
        return write_response(
            stream,
            Response::PaneLog {
                pane_id,
                exit_code: None,
                output,
            },
        );
    }
    let recent = daemon.recent.lock().unwrap();
    let Some(summary) = recent.iter().find(|pane| pane.pane_id == pane_id) else {
        return write_response(
            stream,
            Response::Error {
                message: format!("pane not found: {pane_id}"),
            },
        );
    };
    write_response(
        stream,
        Response::PaneLog {
            pane_id,
            exit_code: summary.exit_code,
            output: summary.output.clone(),
        },
    )
}

fn history_tail(history: &VecDeque<u8>) -> String {
    const TAIL_LIMIT: usize = 8 * 1024;
    let start = history.len().saturating_sub(TAIL_LIMIT);
    String::from_utf8_lossy(
        history
            .iter()
            .skip(start)
            .copied()
            .collect::<Vec<_>>()
            .as_slice(),
    )
    .into_owned()
}

fn attach(
    daemon: &Arc<Daemon>,
    pane_id: String,
    cols: u16,
    rows: u16,
    mut stream: UnixStream,
) -> Result<()> {
    let Some(pane) = daemon.panes.lock().unwrap().get(&pane_id).cloned() else {
        return write_response(
            &mut stream,
            Response::Error {
                message: format!("pane not found: {pane_id}"),
            },
        );
    };
    if let Err(error) = pane._master.lock().unwrap().resize(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        return write_response(
            &mut stream,
            Response::Error {
                message: format!("resize pane failed: {error}"),
            },
        );
    }
    pane.screen.lock().unwrap().resize(rows, cols);
    write_response(&mut stream, Response::Attached { pane_id })?;
    let (sender, receiver) = mpsc::channel();
    let history: Vec<u8> = pane.history.lock().unwrap().iter().copied().collect();
    pane.clients.lock().unwrap().push(sender);
    if !history.is_empty() {
        // A pane can emit its initial UI before a client connects. Replay its
        // bounded output history so attachment redraws the current terminal.
        pane.clients
            .lock()
            .unwrap()
            .last()
            .expect("attached client was just inserted")
            .send(history)?;
    }
    let mut output = stream.try_clone()?;
    thread::spawn(move || {
        while let Ok(data) = receiver.recv() {
            if output.write_all(&data).is_err() {
                break;
            }
        }
        let _ = output.shutdown(Shutdown::Write);
    });
    let mut buffer = [0_u8; 4096];
    loop {
        let size = stream.read(&mut buffer)?;
        if size == 0 {
            return Ok(());
        }
        pane.writer.lock().unwrap().write_all(&buffer[..size])?;
        pane.writer.lock().unwrap().flush()?;
    }
}

fn read_line(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let size = stream.read(&mut byte)?;
        if size == 0 {
            return if line.is_empty() {
                Ok(None)
            } else {
                anyhow::bail!("request ended before its newline")
            };
        }
        if byte[0] == b'\n' {
            return Ok(Some(line));
        }
        line.push(byte[0]);
        if line.len() > 64 * 1024 {
            anyhow::bail!("request line too long");
        }
    }
}

fn write_response(stream: &mut UnixStream, response: Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}
