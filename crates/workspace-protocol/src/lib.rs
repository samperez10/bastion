//! Versioned, local daemon/client protocol for Bastion.
//!
//! Keep this crate free of database and PTY implementation details so clients,
//! adapters, and future tooling can agree on the wire format.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u16 = 1;
/// Increment when a running daemon must be replaced to use a release's
/// server-side behavior. Clients use it for safe stale-daemon detection.
pub const DAEMON_REVISION: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Unknown,
    Idle,
    Working,
    Attention,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    Permission,
    Question,
    Authentication,
    Confirmation,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNotification {
    pub sequence: u64,
    pub pane_id: String,
    pub agent: String,
    pub state: AgentState,
    pub reason: Option<AttentionReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Status,
    ListWorkspaces,
    FocusWorkspace {
        cwd: PathBuf,
    },
    RemoveWorkspace {
        cwd: PathBuf,
        force: bool,
    },
    StartShell {
        cwd: Option<PathBuf>,
        tab: Option<String>,
    },
    StartAgent {
        agent: String,
        slot: String,
        cwd: Option<PathBuf>,
        tab: Option<String>,
    },
    ResumeAgent {
        agent: String,
        slot: String,
        cwd: Option<PathBuf>,
        tab: Option<String>,
    },
    ReportAgentSession {
        agent: String,
        slot: String,
        session_id: String,
        cwd: Option<PathBuf>,
    },
    ReportAgentState {
        agent: String,
        slot: String,
        state: AgentState,
        reason: Option<AttentionReason>,
    },
    ListNotifications {
        after_sequence: u64,
    },
    ListSlots {
        cwd: Option<PathBuf>,
    },
    ListTabs {
        cwd: Option<PathBuf>,
    },
    CreateTab {
        name: String,
        cwd: Option<PathBuf>,
    },
    DeleteTab {
        name: String,
        cwd: Option<PathBuf>,
    },
    PaneLog {
        pane_id: String,
    },
    PaneSnapshot {
        pane_id: String,
    },
    StopPane {
        pane_id: String,
    },
    RenamePane {
        pane_id: String,
        name: String,
    },
    MovePane {
        pane_id: String,
        tab: String,
        cwd: Option<PathBuf>,
    },
    Attach {
        pane_id: String,
        cols: u16,
        rows: u16,
    },
}
