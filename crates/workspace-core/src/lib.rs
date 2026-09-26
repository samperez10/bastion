use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub notifications: NotificationPreferences,
    pub updates: UpdatePreferences,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            notifications: NotificationPreferences::default(),
            updates: UpdatePreferences::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPreferences {
    pub sound: bool,
}

impl Default for NotificationPreferences {
    fn default() -> Self {
        Self { sound: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatePreferences {
    pub automatic_checks: bool,
}

impl Default for UpdatePreferences {
    fn default() -> Self {
        Self {
            automatic_checks: true,
        }
    }
}

impl Preferences {
    pub fn load(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("preferences.json");
        match std::fs::read_to_string(&path) {
            Ok(input) => serde_json::from_str(&input)
                .with_context(|| format!("parse preferences {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("read preferences {}", path.display()))
            }
        }
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create state directory {}", state_dir.display()))?;
        let path = state_dir.join("preferences.json");
        let temporary = state_dir.join("preferences.json.tmp");
        std::fs::write(&temporary, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("write preferences {}", temporary.display()))?;
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("replace preferences {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub canonical_root: String,
    pub git_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSlot {
    pub id: i64,
    pub project_id: i64,
    pub agent_kind: String,
    pub slot_name: String,
    pub native_session_id: Option<String>,
    pub last_state: String,
    pub restore_enabled: bool,
    pub last_tab: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTab {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub position: i64,
}

pub struct StateDb {
    connection: Connection,
}

impl StateDb {
    pub fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create state directory {}", state_dir.display()))?;
        let connection = Connection::open(state_dir.join("workspace.db"))?;
        connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS projects (
                id INTEGER PRIMARY KEY,
                canonical_root TEXT NOT NULL UNIQUE,
                git_dir TEXT,
                remote_hash TEXT,
                last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS session_state (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                last_project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS agent_slots (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                -- Adapter IDs are intentionally open-ended. Bastion manages
                -- every terminal command; adapters add optional resume/state
                -- capabilities without locking the schema to two agents.
                agent_kind TEXT NOT NULL,
                slot_name TEXT NOT NULL,
                config_home_hash TEXT NOT NULL DEFAULT 'default',
                native_session_kind TEXT,
                native_session_value TEXT,
                last_cwd TEXT,
                last_state TEXT NOT NULL DEFAULT 'unknown',
                restore_enabled INTEGER NOT NULL DEFAULT 0,
                last_tab TEXT,
                adapter_version INTEGER,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(project_id, agent_kind, slot_name, config_home_hash)
            );
            CREATE TABLE IF NOT EXISTS workspace_tabs (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                position INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(project_id, name)
            );
            CREATE TABLE IF NOT EXISTS panes (
                id TEXT PRIMARY KEY,
                project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
                tab_id INTEGER REFERENCES workspace_tabs(id) ON DELETE SET NULL,
                slot_id INTEGER REFERENCES agent_slots(id) ON DELETE SET NULL,
                label TEXT,
                command_template TEXT NOT NULL,
                pty_state TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            ",
        )?;
        if !column_exists(&connection, "panes", "tab_id")? {
            connection.execute_batch(
                "ALTER TABLE panes ADD COLUMN tab_id INTEGER REFERENCES workspace_tabs(id) ON DELETE SET NULL;",
            )?;
        }
        if !column_exists(&connection, "panes", "label")? {
            connection.execute_batch("ALTER TABLE panes ADD COLUMN label TEXT;")?;
        }
        // A historical native ID is not proof the user still wants a pane
        // restored. Existing records opt out until a current session is seen.
        if !column_exists(&connection, "agent_slots", "restore_enabled")? {
            connection.execute_batch(
                "ALTER TABLE agent_slots ADD COLUMN restore_enabled INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if !column_exists(&connection, "agent_slots", "last_tab")? {
            connection.execute_batch("ALTER TABLE agent_slots ADD COLUMN last_tab TEXT;")?;
        }
        migrate_restricted_agent_slots(&connection)?;
        Ok(Self { connection })
    }

    pub fn ensure_project(&self, cwd: &Path) -> Result<Project> {
        let root = project_root(cwd)?;
        let root_text = root.to_string_lossy().into_owned();
        let git_dir = git_dir(&root);
        self.connection.execute(
            "INSERT INTO projects (canonical_root, git_dir) VALUES (?1, ?2)
             ON CONFLICT(canonical_root) DO UPDATE SET git_dir = excluded.git_dir,
               last_seen_at = CURRENT_TIMESTAMP",
            params![root_text, git_dir],
        )?;
        let project: Project = self.connection.query_row(
            "SELECT id, canonical_root, git_dir FROM projects WHERE canonical_root = ?1",
            [&root.to_string_lossy()],
            |row| {
                Ok(Project {
                    id: row.get(0)?,
                    canonical_root: row.get(1)?,
                    git_dir: row.get(2)?,
                })
            },
        )?;
        self.ensure_tab(&project, "Main")?;
        Ok(project)
    }

    /// Find an already registered workspace without creating it as a side
    /// effect. This is used for destructive state operations such as removal.
    pub fn project_at(&self, cwd: &Path) -> Result<Option<Project>> {
        let root = project_root(cwd)?;
        self.connection
            .query_row(
                "SELECT id, canonical_root, git_dir FROM projects WHERE canonical_root = ?1",
                [&root.to_string_lossy()],
                |row| {
                    Ok(Project {
                        id: row.get(0)?,
                        canonical_root: row.get(1)?,
                        git_dir: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Remove Bastion's state for a workspace. Project files are never part of
    /// this operation. Panes are deleted explicitly because their foreign key
    /// intentionally permits a project to be removed independently.
    pub fn delete_project(&self, project: &Project) -> Result<()> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            self.connection
                .execute("DELETE FROM panes WHERE project_id = ?1", [project.id])?;
            let removed = self
                .connection
                .execute("DELETE FROM projects WHERE id = ?1", [project.id])?;
            if removed != 1 {
                anyhow::bail!("workspace no longer exists in Bastion state")
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.connection.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
        Ok(())
    }

    /// Workspaces known to the local Bastion session, most recently used first.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut statement = self.connection.prepare(
            "SELECT id, canonical_root, git_dir FROM projects ORDER BY last_seen_at DESC, id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                canonical_root: row.get(1)?,
                git_dir: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn focus_project(&self, project: &Project) -> Result<()> {
        self.connection.execute(
            "INSERT INTO session_state (id, last_project_id) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET last_project_id = excluded.last_project_id,
               updated_at = CURRENT_TIMESTAMP",
            [project.id],
        )?;
        Ok(())
    }

    pub fn focused_project(&self) -> Result<Option<Project>> {
        self.connection
            .query_row(
                "SELECT p.id, p.canonical_root, p.git_dir
                 FROM session_state s JOIN projects p ON p.id = s.last_project_id
                 WHERE s.id = 1",
                [],
                |row| {
                    Ok(Project {
                        id: row.get(0)?,
                        canonical_root: row.get(1)?,
                        git_dir: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn ensure_tab(&self, project: &Project, name: &str) -> Result<WorkspaceTab> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("tab name cannot be empty");
        }
        let next_position: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM workspace_tabs WHERE project_id = ?1",
            [project.id],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT INTO workspace_tabs (project_id, name, position) VALUES (?1, ?2, ?3)
             ON CONFLICT(project_id, name) DO NOTHING",
            params![project.id, name, next_position],
        )?;
        self.connection
            .query_row(
                "SELECT id, project_id, name, position FROM workspace_tabs
             WHERE project_id = ?1 AND name = ?2",
                params![project.id, name],
                |row| {
                    Ok(WorkspaceTab {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        name: row.get(2)?,
                        position: row.get(3)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn list_tabs(&self, project: &Project) -> Result<Vec<WorkspaceTab>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, name, position FROM workspace_tabs
             WHERE project_id = ?1 ORDER BY position, id",
        )?;
        let rows = statement.query_map([project.id], |row| {
            Ok(WorkspaceTab {
                id: row.get(0)?,
                project_id: row.get(1)?,
                name: row.get(2)?,
                position: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn active_pane_count(&self, project: &Project) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM panes WHERE project_id = ?1 AND pty_state != 'stopped'",
                [project.id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn delete_tab(&self, project: &Project, name: &str) -> Result<()> {
        if name == "Main" {
            anyhow::bail!("the Main tab cannot be deleted");
        }
        let changed = self.connection.execute(
            "DELETE FROM workspace_tabs WHERE project_id = ?1 AND name = ?2",
            params![project.id, name],
        )?;
        if changed == 0 {
            anyhow::bail!("tab not found: {name}");
        }
        Ok(())
    }

    pub fn ensure_slot(&self, project: &Project, agent: &str, slot: &str) -> Result<AgentSlot> {
        if agent.trim().is_empty() || slot.trim().is_empty() {
            anyhow::bail!("agent and slot names cannot be empty");
        }
        self.connection.execute(
            "INSERT INTO agent_slots (project_id, agent_kind, slot_name) VALUES (?1, ?2, ?3)
             ON CONFLICT(project_id, agent_kind, slot_name, config_home_hash) DO NOTHING",
            params![project.id, agent, slot],
        )?;
        self.connection
            .query_row(
                "SELECT id, project_id, agent_kind, slot_name, native_session_value, last_state, restore_enabled, last_tab
             FROM agent_slots WHERE project_id = ?1 AND agent_kind = ?2 AND slot_name = ?3
             AND config_home_hash = 'default'",
                params![project.id, agent, slot],
                |row| {
                    Ok(AgentSlot {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        agent_kind: row.get(2)?,
                        slot_name: row.get(3)?,
                        native_session_id: row.get(4)?,
                        last_state: row.get(5)?,
                        restore_enabled: row.get::<_, i64>(6)? != 0,
                        last_tab: row.get(7)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn list_slots(&self, project: &Project) -> Result<Vec<AgentSlot>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, agent_kind, slot_name, native_session_value, last_state, restore_enabled, last_tab
             FROM agent_slots WHERE project_id = ?1 ORDER BY agent_kind, slot_name",
        )?;
        let rows = statement.query_map([project.id], |row| {
            Ok(AgentSlot {
                id: row.get(0)?,
                project_id: row.get(1)?,
                agent_kind: row.get(2)?,
                slot_name: row.get(3)?,
                native_session_id: row.get(4)?,
                last_state: row.get(5)?,
                restore_enabled: row.get::<_, i64>(6)? != 0,
                last_tab: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Slots deliberately retained for recovery after the daemon goes away.
    ///
    /// A `pane-*` slot is the identity of a user-created terminal pane.  It is
    /// safe to restore once its owning pane has recorded an agent session:
    /// explicit pane removal clears that slot before the next daemon start.
    pub fn restorable_slots(&self, project: &Project) -> Result<Vec<AgentSlot>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, agent_kind, slot_name, native_session_value, last_state, restore_enabled, last_tab
             FROM agent_slots
             WHERE project_id = ?1
               AND restore_enabled = 1
               AND native_session_value IS NOT NULL
               AND last_state != 'stopped'
             ORDER BY updated_at, id",
        )?;
        let rows = statement.query_map([project.id], |row| {
            Ok(AgentSlot {
                id: row.get(0)?,
                project_id: row.get(1)?,
                agent_kind: row.get(2)?,
                slot_name: row.get(3)?,
                native_session_id: row.get(4)?,
                last_state: row.get(5)?,
                restore_enabled: row.get::<_, i64>(6)? != 0,
                last_tab: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn forget_slot(&self, project: &Project, agent: &str, slot: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE agent_slots SET native_session_value = NULL, last_state = 'stopped', restore_enabled = 0,
             updated_at = CURRENT_TIMESTAMP
             WHERE project_id = ?1 AND agent_kind = ?2 AND slot_name = ?3 AND config_home_hash = 'default'",
            params![project.id, agent, slot],
        )?;
        Ok(())
    }

    pub fn slot(&self, project: &Project, agent: &str, slot: &str) -> Result<Option<AgentSlot>> {
        self.connection
            .query_row(
                "SELECT id, project_id, agent_kind, slot_name, native_session_value, last_state, restore_enabled, last_tab
                 FROM agent_slots WHERE project_id = ?1 AND agent_kind = ?2 AND slot_name = ?3
                 AND config_home_hash = 'default'",
                params![project.id, agent, slot],
                |row| {
                    Ok(AgentSlot {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        agent_kind: row.get(2)?,
                        slot_name: row.get(3)?,
                        native_session_id: row.get(4)?,
                        last_state: row.get(5)?,
                        restore_enabled: row.get::<_, i64>(6)? != 0,
                        last_tab: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn record_agent_session(
        &self,
        project: &Project,
        agent: &str,
        slot: &str,
        session_id: &str,
    ) -> Result<AgentSlot> {
        let agent_slot = self.ensure_slot(project, agent, slot)?;
        self.connection.execute(
            "UPDATE agent_slots SET native_session_kind = 'id', native_session_value = ?1,
             last_state = 'idle', restore_enabled = 1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
            params![session_id, agent_slot.id],
        )?;
        self.slot(project, agent, slot)?
            .context("agent slot disappeared while recording a session")
    }

    pub fn set_slot_tab(
        &self,
        project: &Project,
        agent: &str,
        slot: &str,
        tab: &str,
    ) -> Result<()> {
        self.ensure_slot(project, agent, slot)?;
        self.connection.execute(
            "UPDATE agent_slots SET last_tab = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE project_id = ?2 AND agent_kind = ?3 AND slot_name = ?4 AND config_home_hash = 'default'",
            params![tab, project.id, agent, slot],
        )?;
        Ok(())
    }

    pub fn record_pane(
        &self,
        pane_id: &str,
        project_id: i64,
        tab_id: i64,
        label: &str,
        command: &str,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO panes (id, project_id, tab_id, label, command_template, pty_state)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running')
             ON CONFLICT(id) DO UPDATE SET pty_state = 'running', label = excluded.label,
               updated_at = CURRENT_TIMESTAMP",
            params![pane_id, project_id, tab_id, label, command],
        )?;
        Ok(())
    }

    pub fn rename_pane(&self, pane_id: &str, label: &str) -> Result<()> {
        let label = label.trim();
        if label.is_empty() {
            anyhow::bail!("pane name cannot be empty");
        }
        if label.chars().count() > 48 {
            anyhow::bail!("pane name must be 48 characters or fewer");
        }
        let changed = self.connection.execute(
            "UPDATE panes SET label = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
            params![label, pane_id],
        )?;
        if changed == 0 {
            anyhow::bail!("pane not found: {pane_id}");
        }
        Ok(())
    }

    pub fn move_pane(
        &self,
        pane_id: &str,
        project: &Project,
        tab_name: &str,
    ) -> Result<WorkspaceTab> {
        let tab = self.ensure_tab(project, tab_name)?;
        let changed = self.connection.execute(
            "UPDATE panes SET tab_id = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND project_id = ?3",
            params![tab.id, pane_id, project.id],
        )?;
        if changed == 0 {
            anyhow::bail!("pane not found in this workspace: {pane_id}");
        }
        Ok(tab)
    }

    pub fn mark_pane_stopped(&self, pane_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE panes SET pty_state = 'stopped', updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            [pane_id],
        )?;
        Ok(())
    }

    /// A daemon restart cannot retain its old PTYs. Clear their persisted
    /// running markers before any deliberate restore creates new panes.
    pub fn mark_all_panes_stopped(&self) -> Result<()> {
        self.connection.execute(
            "UPDATE panes SET pty_state = 'stopped', updated_at = CURRENT_TIMESTAMP
             WHERE pty_state != 'stopped'",
            [],
        )?;
        Ok(())
    }

    /// `pane-<uuid>` is an internal detection token for a manually started
    /// shell, not a stable user-selected agent slot. It can observe more than
    /// one agent over its lifetime, so it must never be an auto-restore target.
    pub fn quarantine_tracking_slots(&self) -> Result<()> {
        self.connection.execute(
            "UPDATE agent_slots SET restore_enabled = 0, updated_at = CURRENT_TIMESTAMP
             WHERE slot_name LIKE 'pane-%' AND restore_enabled != 0",
            [],
        )?;
        Ok(())
    }
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for candidate in columns {
        if candidate? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Early Bastion releases constrained `agent_kind` to Claude and Codex.
/// SQLite cannot remove a CHECK constraint in place, so rebuild only the two
/// metadata tables that reference it. This is atomic and never touches a
/// project directory or its files.
fn migrate_restricted_agent_slots(connection: &Connection) -> Result<()> {
    let schema: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'agent_slots'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(schema) = schema else {
        return Ok(());
    };
    let normalized = schema.to_ascii_lowercase();
    if !normalized.contains("agent_kind in ('claude', 'codex')") {
        return Ok(());
    }

    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let result = connection.execute_batch(
        "
        BEGIN IMMEDIATE;
        CREATE TABLE agent_slots_migrated (
            id INTEGER PRIMARY KEY,
            project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            agent_kind TEXT NOT NULL,
            slot_name TEXT NOT NULL,
            config_home_hash TEXT NOT NULL DEFAULT 'default',
            native_session_kind TEXT,
            native_session_value TEXT,
            last_cwd TEXT,
            last_state TEXT NOT NULL DEFAULT 'unknown',
            restore_enabled INTEGER NOT NULL DEFAULT 0,
            last_tab TEXT,
            adapter_version INTEGER,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(project_id, agent_kind, slot_name, config_home_hash)
        );
        INSERT INTO agent_slots_migrated (
            id, project_id, agent_kind, slot_name, config_home_hash,
            native_session_kind, native_session_value, last_cwd, last_state,
            restore_enabled, last_tab, adapter_version, updated_at
        )
        SELECT id, project_id, agent_kind, slot_name, config_home_hash,
               native_session_kind, native_session_value, last_cwd, last_state,
               restore_enabled, last_tab, adapter_version, updated_at
          FROM agent_slots;

        CREATE TABLE panes_migrated (
            id TEXT PRIMARY KEY,
            project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            tab_id INTEGER REFERENCES workspace_tabs(id) ON DELETE SET NULL,
            slot_id INTEGER REFERENCES agent_slots(id) ON DELETE SET NULL,
            label TEXT,
            command_template TEXT NOT NULL,
            pty_state TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        INSERT INTO panes_migrated (
            id, project_id, tab_id, slot_id, label, command_template, pty_state, updated_at
        )
        SELECT id, project_id, tab_id, slot_id, label, command_template, pty_state, updated_at
          FROM panes;
        DROP TABLE panes;
        DROP TABLE agent_slots;
        ALTER TABLE agent_slots_migrated RENAME TO agent_slots;
        ALTER TABLE panes_migrated RENAME TO panes;
        COMMIT;
        ",
    );
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK;");
    }
    let foreign_keys = connection.execute_batch("PRAGMA foreign_keys = ON;");
    result?;
    foreign_keys?;
    Ok(())
}

fn project_root(cwd: &Path) -> Result<PathBuf> {
    let canonical = cwd
        .canonicalize()
        .with_context(|| format!("resolve {}", cwd.display()))?;
    let output = std::process::Command::new("git")
        .args([
            "-C",
            canonical.to_string_lossy().as_ref(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !root.is_empty() {
            return PathBuf::from(root).canonicalize().map_err(Into::into);
        }
    }
    Ok(canonical)
}

fn git_dir(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            root.to_string_lossy().as_ref(),
            "rev-parse",
            "--absolute-git-dir",
        ])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_preferences_enable_update_checks_by_default() {
        let preferences: Preferences =
            serde_json::from_str(r#"{"notifications":{"sound":false}}"#).unwrap();
        assert!(!preferences.notifications.sound);
        assert!(preferences.updates.automatic_checks);
    }

    #[test]
    fn accepts_an_unregistered_adapter_slot() {
        let root = std::env::temp_dir().join(format!("bastion-core-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let state_dir = root.join("state");
        {
            let database = StateDb::open(&state_dir).unwrap();
            let project = database.ensure_project(&root).unwrap();
            database.focus_project(&project).unwrap();
            assert_eq!(database.focused_project().unwrap().unwrap().id, project.id);
            let slot = database.ensure_slot(&project, "gemini", "primary").unwrap();
            assert_eq!(slot.agent_kind, "gemini");
            assert_eq!(slot.slot_name, "primary");
            database
                .set_slot_tab(&project, "gemini", "primary", "Research")
                .unwrap();
            assert_eq!(
                database
                    .slot(&project, "gemini", "primary")
                    .unwrap()
                    .unwrap()
                    .last_tab
                    .as_deref(),
                Some("Research")
            );
            database
                .record_agent_session(&project, "codex", "pane-test", "session-id")
                .unwrap();
            database.quarantine_tracking_slots().unwrap();
            assert!(database.restorable_slots(&project).unwrap().is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_a_restricted_agent_schema_without_losing_slots() {
        let root = std::env::temp_dir().join(format!("bastion-migrate-{}", uuid::Uuid::new_v4()));
        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let database_path = state_dir.join("workspace.db");
        let legacy = Connection::open(&database_path).unwrap();
        legacy.execute_batch(
            "
            CREATE TABLE projects (id INTEGER PRIMARY KEY, canonical_root TEXT NOT NULL UNIQUE, git_dir TEXT, remote_hash TEXT, last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
            CREATE TABLE session_state (id INTEGER PRIMARY KEY CHECK(id = 1), last_project_id INTEGER, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
            CREATE TABLE agent_slots (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                agent_kind TEXT NOT NULL CHECK(agent_kind IN ('claude', 'codex')),
                slot_name TEXT NOT NULL,
                config_home_hash TEXT NOT NULL DEFAULT 'default',
                native_session_kind TEXT,
                native_session_value TEXT,
                last_cwd TEXT,
                last_state TEXT NOT NULL DEFAULT 'unknown',
                restore_enabled INTEGER NOT NULL DEFAULT 0,
                last_tab TEXT,
                adapter_version INTEGER,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(project_id, agent_kind, slot_name, config_home_hash)
            );
            CREATE TABLE workspace_tabs (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, name TEXT NOT NULL, position INTEGER NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, UNIQUE(project_id, name));
            CREATE TABLE panes (id TEXT PRIMARY KEY, project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL, tab_id INTEGER REFERENCES workspace_tabs(id) ON DELETE SET NULL, slot_id INTEGER REFERENCES agent_slots(id) ON DELETE SET NULL, label TEXT, command_template TEXT NOT NULL, pty_state TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
            INSERT INTO projects (id, canonical_root) VALUES (1, '/legacy');
            INSERT INTO agent_slots (id, project_id, agent_kind, slot_name, native_session_value, restore_enabled) VALUES (1, 1, 'claude', 'primary', 'old-session', 1);
            INSERT INTO workspace_tabs (id, project_id, name, position) VALUES (1, 1, 'Main', 0);
            INSERT INTO panes (id, project_id, tab_id, slot_id, command_template, pty_state) VALUES ('old-pane', 1, 1, 1, 'claude', 'stopped');
            ",
        ).unwrap();
        drop(legacy);

        let database = StateDb::open(&state_dir).unwrap();
        let project = database
            .list_projects()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            database
                .slot(&project, "claude", "primary")
                .unwrap()
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("old-session")
        );
        assert_eq!(
            database
                .ensure_slot(&project, "antigravity", "primary")
                .unwrap()
                .agent_kind,
            "antigravity"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
