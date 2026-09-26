use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind, poll, read,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
    },
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear as WidgetClear, List, ListItem, ListState, Paragraph, Wrap},
};
use semver::Version;
use serde::Deserialize;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    process::Command as ProcessCommand,
    sync::mpsc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};
use workspace_core::Preferences;
use workspace_protocol::Request;
use workspace_terminal::MouseProtocol;

#[derive(Clone, Copy)]
struct Theme {
    name: &'static str,
    accent: Color,
    border: Color,
    muted: Color,
    focus_background: Color,
    success: Color,
    warning: Color,
    error: Color,
}

const THEMES: [Theme; 7] = [
    Theme {
        name: "mono",
        accent: Color::White,
        border: Color::Gray,
        muted: Color::DarkGray,
        focus_background: Color::DarkGray,
        success: Color::Green,
        warning: Color::Yellow,
        error: Color::Red,
    },
    Theme {
        name: "cyan",
        accent: Color::Rgb(55, 225, 220),
        border: Color::Rgb(35, 140, 145),
        muted: Color::Rgb(100, 145, 150),
        focus_background: Color::Rgb(15, 76, 82),
        success: Color::Rgb(90, 220, 150),
        warning: Color::Rgb(250, 210, 100),
        error: Color::Rgb(250, 110, 120),
    },
    Theme {
        name: "amber",
        accent: Color::Rgb(255, 190, 75),
        border: Color::Rgb(165, 112, 30),
        muted: Color::Rgb(165, 135, 90),
        focus_background: Color::Rgb(82, 54, 14),
        success: Color::Rgb(150, 220, 105),
        warning: Color::Rgb(255, 190, 75),
        error: Color::Rgb(250, 105, 85),
    },
    Theme {
        name: "violet",
        accent: Color::Rgb(190, 145, 255),
        border: Color::Rgb(115, 80, 180),
        muted: Color::Rgb(150, 130, 180),
        focus_background: Color::Rgb(58, 38, 95),
        success: Color::Rgb(110, 220, 170),
        warning: Color::Rgb(255, 205, 115),
        error: Color::Rgb(255, 115, 155),
    },
    Theme {
        name: "forest",
        accent: Color::Rgb(105, 215, 145),
        border: Color::Rgb(55, 130, 85),
        muted: Color::Rgb(110, 155, 120),
        focus_background: Color::Rgb(22, 70, 43),
        success: Color::Rgb(105, 215, 145),
        warning: Color::Rgb(235, 205, 90),
        error: Color::Rgb(245, 110, 105),
    },
    Theme {
        name: "rose",
        accent: Color::Rgb(255, 135, 180),
        border: Color::Rgb(175, 75, 120),
        muted: Color::Rgb(180, 125, 150),
        focus_background: Color::Rgb(88, 36, 61),
        success: Color::Rgb(120, 220, 170),
        warning: Color::Rgb(255, 205, 110),
        error: Color::Rgb(255, 105, 125),
    },
    Theme {
        name: "high-contrast",
        accent: Color::White,
        border: Color::White,
        muted: Color::Gray,
        focus_background: Color::Blue,
        success: Color::Green,
        warning: Color::Yellow,
        error: Color::Red,
    },
];
static ACTIVE_THEME: OnceLock<Mutex<Theme>> = OnceLock::new();

fn accent() -> Color {
    active_theme().accent
}

fn active_theme() -> Theme {
    ACTIVE_THEME
        .get_or_init(|| Mutex::new(THEMES[0]))
        .lock()
        .unwrap()
        .to_owned()
}

fn border() -> Color {
    active_theme().border
}

fn muted() -> Color {
    active_theme().muted
}

fn focus_background() -> Color {
    active_theme().focus_background
}

fn success() -> Color {
    active_theme().success
}

fn warning() -> Color {
    active_theme().warning
}

fn error() -> Color {
    active_theme().error
}

fn theme_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("theme.txt")
}

fn find_theme(name: &str) -> Option<Theme> {
    let name = match name {
        // Preserve old persisted settings from the prototype theme picker.
        "terminal" => "mono",
        "nord" => "cyan",
        "tokyo-night" => "violet",
        "gruvbox" => "amber",
        value => value,
    };
    THEMES.iter().copied().find(|theme| theme.name == name)
}

fn set_theme(theme: Theme) {
    *ACTIVE_THEME
        .get_or_init(|| Mutex::new(THEMES[0]))
        .lock()
        .unwrap() = theme;
}

fn load_theme(state_dir: &std::path::Path) {
    let Ok(name) = std::fs::read_to_string(theme_path(state_dir)) else {
        return;
    };
    if let Some(theme) = find_theme(name.trim()) {
        set_theme(theme);
    }
}

fn theme_command(state_dir: &PathBuf, name: Option<&str>) -> Result<()> {
    match name {
        None | Some("list") => {
            println!(
                "{}",
                THEMES
                    .iter()
                    .map(|theme| theme.name)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        Some(name) => {
            let theme = find_theme(name).context("unknown theme; run `termux-tui theme list`")?;
            std::fs::create_dir_all(state_dir)?;
            std::fs::write(theme_path(state_dir), format!("{}\n", theme.name))?;
            set_theme(theme);
            println!("theme: {}", theme.name);
        }
    }
    Ok(())
}

fn cycle_theme(state_dir: &PathBuf) -> Result<&'static str> {
    let current = *ACTIVE_THEME
        .get_or_init(|| Mutex::new(THEMES[1]))
        .lock()
        .unwrap();
    let index = THEMES
        .iter()
        .position(|theme| theme.name == current.name)
        .unwrap_or(0);
    let next = THEMES[(index + 1) % THEMES.len()];
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(theme_path(state_dir), format!("{}\n", next.name))?;
    set_theme(next);
    Ok(next.name)
}

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_os_t = default_state_dir())]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/termux-agent-workspace")
}

#[derive(Subcommand)]
enum Command {
    Status,
    StartShell {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        tab: Option<String>,
    },
    StartAgent {
        agent: String,
        #[arg(long, default_value = "primary")]
        slot: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        tab: Option<String>,
    },
    ResumeAgent {
        agent: String,
        #[arg(long, default_value = "primary")]
        slot: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        tab: Option<String>,
    },
    ReportSession {
        agent: String,
        session_id: String,
        #[arg(long, default_value = "primary")]
        slot: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    Slots {
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    Tabs {
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    NewTab {
        name: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Remove Bastion state for a workspace without touching project files.
    RemoveWorkspace {
        cwd: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Show the project, tabs, live panes, and resumable agent slots together.
    Dashboard {
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Open the remembered-workspace switcher, then a selected dashboard.
    Session,
    Logs {
        pane_id: String,
    },
    Attach {
        pane_id: String,
    },
    RenamePane {
        pane_id: String,
        name: String,
    },
    MovePane {
        pane_id: String,
        tab: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Show or set the workspace chrome theme.
    Theme {
        name: Option<String>,
    },
}

fn main() -> Result<()> {
    let Args { state_dir, command } = Args::parse();
    if let Command::Theme { name } = command {
        return theme_command(&state_dir, name.as_deref());
    }
    if let Command::Dashboard { cwd } = command {
        return dashboard(&state_dir, cwd.as_ref());
    }
    if let Command::Session = command {
        return session_dashboard(&state_dir);
    }
    let mut stream = UnixStream::connect(state_dir.join("workspace.sock"))?;
    match command {
        Command::Status => request(&mut stream, Request::Status),
        Command::StartShell { cwd, tab } => request(&mut stream, Request::StartShell { cwd, tab }),
        Command::StartAgent {
            agent,
            slot,
            cwd,
            tab,
        } => request(
            &mut stream,
            Request::StartAgent {
                agent,
                slot,
                cwd,
                tab,
            },
        ),
        Command::ResumeAgent {
            agent,
            slot,
            cwd,
            tab,
        } => request(
            &mut stream,
            Request::ResumeAgent {
                agent,
                slot,
                cwd,
                tab,
            },
        ),
        Command::ReportSession {
            agent,
            session_id,
            slot,
            cwd,
        } => request(
            &mut stream,
            Request::ReportAgentSession {
                agent,
                slot,
                session_id,
                cwd,
            },
        ),
        Command::Slots { cwd } => request(&mut stream, Request::ListSlots { cwd }),
        Command::Tabs { cwd } => request(&mut stream, Request::ListTabs { cwd }),
        Command::NewTab { name, cwd } => request(&mut stream, Request::CreateTab { name, cwd }),
        Command::RemoveWorkspace { cwd, force } => {
            remove_workspace_command(&mut stream, cwd, force)
        }
        Command::Logs { pane_id } => logs(&mut stream, &pane_id),
        Command::RenamePane { pane_id, name } => {
            request(&mut stream, Request::RenamePane { pane_id, name })
        }
        Command::MovePane { pane_id, tab, cwd } => {
            request(&mut stream, Request::MovePane { pane_id, tab, cwd })
        }
        Command::Dashboard { .. } => unreachable!("dashboard returns before connecting"),
        Command::Session => unreachable!("session returns before connecting"),
        Command::Attach { pane_id } => attach(&mut stream, &pane_id),
        Command::Theme { .. } => unreachable!("handled before daemon connection"),
    }
}

fn dashboard(state_dir: &PathBuf, cwd: Option<&PathBuf>) -> Result<()> {
    load_theme(state_dir);
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Request clipboard pastes from the outer terminal as one Event::Paste.
    // Without this, Android/Termux sends one key event per character, which
    // makes a paste look like slow typing and can turn embedded newlines into
    // accidental submits in agent TUIs.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let notification_listener = NotificationListener::start(state_dir.clone());
    let result = dashboard_in_session(&mut terminal, state_dir, cwd, &notification_listener);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    result
}

fn dashboard_in_session(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state_dir: &PathBuf,
    cwd: Option<&PathBuf>,
    notification_listener: &NotificationListener,
) -> Result<()> {
    let workspace = cwd
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut remembered_tab = 0_usize;
    let mut notice = String::new();
    let result = loop {
        let exit = match catch_unwind(AssertUnwindSafe(|| {
            window_dashboard_loop(
                terminal,
                state_dir,
                &workspace,
                &mut remembered_tab,
                &mut notice,
            )
        })) {
            Ok(Ok(exit)) => exit,
            Ok(Err(error)) => {
                // Keep the alternate-screen session alive. A transient daemon
                // failure must not strand the user outside their workspace.
                notice = format!("dashboard recovered: {error:#}");
                continue;
            }
            Err(_) => {
                notice = "dashboard renderer recovered; workspace is still available".to_owned();
                continue;
            }
        };
        match exit {
            DashboardExit::Quit => break Ok(()),
            DashboardExit::Attach {
                pane_id,
                label,
                preview,
            } => {
                let mut active_id = pane_id;
                let mut active_label = label;
                let mut preview = preview;
                notification_listener.set_active(Some(active_id.clone()));
                loop {
                    match catch_unwind(AssertUnwindSafe(|| {
                        embedded_pane(
                            terminal,
                            state_dir,
                            &active_id,
                            &active_label,
                            preview.as_ref(),
                        )
                    })) {
                        Err(_) => {
                            notice = "pane renderer recovered; returned to workspace".to_owned();
                            break;
                        }
                        Ok(Err(error)) => {
                            notice = format!("returned to workspace: {error:#}");
                            break;
                        }
                        Ok(Ok(EmbeddedPaneExit::Workspace)) => break,
                        // Termux reports this for rotation, split-screen,
                        // font-scale, and IME layout changes. Reattaching
                        // recalculates the viewport and makes the daemon
                        // resize the PTY before a fresh grid is drawn.
                        Ok(Ok(EmbeddedPaneExit::Reattach)) => continue,
                        Ok(Ok(EmbeddedPaneExit::Focus(next_id))) => {
                            let old = PanePreview {
                                pane_id: active_id,
                                label: active_label,
                            };
                            active_label = preview
                                .as_ref()
                                .map(|pane| pane.label.clone())
                                .unwrap_or_else(|| "Active pane".to_owned());
                            active_id = next_id;
                            notification_listener.set_active(Some(active_id.clone()));
                            preview = Some(old);
                        }
                    }
                }
                notification_listener.set_active(None);
            }
        }
    };
    result
}

/// The outer Bastion session switcher. Closing a workspace dashboard returns
/// here instead of losing the user's multi-project context.
fn session_dashboard(state_dir: &PathBuf) -> Result<()> {
    load_theme(state_dir);
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let notification_listener = NotificationListener::start(state_dir.clone());
    let mut install_update = false;
    let result = (|| -> Result<()> {
        while let Some(choice) = pick_workspace(&mut terminal, state_dir)? {
            match choice {
                SessionChoice::Workspace(workspace) => {
                    dashboard_request(
                        state_dir,
                        Request::FocusWorkspace {
                            cwd: workspace.clone(),
                        },
                    )?;
                    dashboard_in_session(
                        &mut terminal,
                        state_dir,
                        Some(&workspace),
                        &notification_listener,
                    )?;
                }
                SessionChoice::InstallUpdate => {
                    install_update = true;
                    break;
                }
            }
        }
        Ok(())
    })();
    drop(notification_listener);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    result?;
    if install_update {
        let status = ProcessCommand::new(sibling_binary("bastion"))
            .args([
                "--state-dir",
                state_dir.to_string_lossy().as_ref(),
                "update",
                "install",
                "--yes",
            ])
            .status()
            .context("install Bastion update")?;
        if !status.success() {
            anyhow::bail!("Bastion update exited with {status}");
        }
    }
    Ok(())
}

enum SessionChoice {
    Workspace(PathBuf),
    InstallUpdate,
}

fn pick_workspace(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state_dir: &PathBuf,
) -> Result<Option<SessionChoice>> {
    let response = dashboard_request(state_dir, Request::ListWorkspaces)?;
    let mut workspaces = response
        .get("workspaces")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if workspaces.is_empty() {
        return Ok(None);
    }
    let mut selected = 0_usize;
    let mut available_update = cached_update_version(state_dir);
    let mut hint = workspace_hint(available_update.as_deref());
    let mut removing = false;
    let mut settings_open = false;
    let mut update_confirmation = false;
    let mut selected_setting = 0_usize;
    let mut sound_enabled = Preferences::load(state_dir)?.notifications.sound;
    let result = loop {
        let refreshed_update = cached_update_version(state_dir);
        if refreshed_update != available_update
            && !settings_open
            && !removing
            && !update_confirmation
        {
            hint = workspace_hint(refreshed_update.as_deref());
        }
        available_update = refreshed_update;
        terminal.draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(10),
                    Constraint::Min(5),
                    Constraint::Length(2),
                ])
                .split(frame.area());
            frame.render_widget(workspace_masthead(), areas[0]);
            let items = workspaces
                .iter()
                .map(|workspace| {
                    let root = workspace
                        .get("canonical_root")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let label = PathBuf::from(root)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(root)
                        .to_owned();
                    let panes = workspace
                        .get("pane_count")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let agent = workspace
                        .get("agent_summary")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("shell");
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!(" {label}"),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  {panes} panes · {agent}"),
                            Style::default().fg(muted()),
                        ),
                    ]))
                })
                .collect::<Vec<_>>();
            let mut state = ListState::default();
            state.select(Some(selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(border()))
                            .title(" BASTION · WORKSPACES "),
                    )
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
                areas[1],
                &mut state,
            );
            frame.render_widget(
                Paragraph::new(if removing {
                    " Removal confirmation open "
                } else if settings_open {
                    " Settings · tap a row · Esc closes "
                } else {
                    hint.as_str()
                })
                    .style(Style::default().fg(muted()))
                    .block(Block::default().borders(Borders::TOP)),
                areas[2],
            );
            if removing {
                let dialog = centered_rect(90, 55, frame.area());
                let root = workspaces[selected].get("canonical_root").and_then(serde_json::Value::as_str).unwrap_or("?");
                let panes = workspace_pane_count(&workspaces[selected]);
                frame.render_widget(WidgetClear, dialog);
                frame.render_widget(
                    Paragraph::new(format!(
                        "Remove this Bastion workspace?\n  {root}\n\nBastion removes tabs, pane records, and saved sessions.\nYour project folder and files are untouched.\n\n{}\n\n  CANCEL                         {}",
                        if panes == 0 { "No running panes." } else { "Running panes will be stopped." },
                        if panes == 0 { "REMOVE" } else { "STOP & REMOVE" }
                    ))
                    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(error())).title(" Remove workspace · Enter confirms · Esc cancels "))
                    .wrap(Wrap { trim: true }),
                    dialog,
                );
            }
            if settings_open {
                let dialog = centered_fixed(50, 8, frame.area());
                let selected_style = Style::default()
                    .fg(accent())
                    .bg(focus_background())
                    .add_modifier(Modifier::BOLD);
                let normal_style = Style::default().fg(Color::White);
                let sound = format!(
                    "{} Sound notifications       {}",
                    if selected_setting == 0 { "›" } else { " " },
                    if sound_enabled { "ON" } else { "OFF" }
                );
                let theme = format!(
                    "{} Theme                 {}",
                    if selected_setting == 1 { "›" } else { " " },
                    active_theme().name.to_ascii_uppercase()
                );
                let updates = format!(
                    "{} Updates                 {}",
                    if selected_setting == 2 { "›" } else { " " },
                    available_update
                        .as_deref()
                        .map(|version| format!("v{version}"))
                        .unwrap_or_else(|| "CURRENT".to_owned())
                );
                frame.render_widget(WidgetClear, dialog);
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled(sound, if selected_setting == 0 { selected_style } else { normal_style }),
                        Line::styled(theme, if selected_setting == 1 { selected_style } else { normal_style }),
                        Line::styled(updates, if selected_setting == 2 { selected_style } else { normal_style }),
                        Line::raw(""),
                        Line::styled("  Esc · back", Style::default().fg(muted())),
                    ])
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(border()))
                            .title(" BASTION · SETTINGS "),
                    ),
                    dialog,
                );
            }
            if update_confirmation {
                let dialog = centered_fixed(54, 10, frame.area());
                let version = available_update.as_deref().unwrap_or("new release");
                frame.render_widget(WidgetClear, dialog);
                frame.render_widget(
                    Paragraph::new(format!(
                        "Install Bastion v{version}?\n\nThe release will be checksum-verified. A running daemon will restart; saved agent sessions remain available.\n\n  LATER          X SKIP          ENTER UPDATE"
                    ))
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(accent()))
                            .title(" BASTION UPDATE "),
                    ),
                    dialog,
                );
            }
        })?;
        match read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc if update_confirmation => update_confirmation = false,
                KeyCode::Char('x') if update_confirmation => {
                    match skip_cached_update(state_dir) {
                        Ok(()) => hint = " Update skipped until a newer release ".to_owned(),
                        Err(error) => hint = format!(" Could not skip update: {error:#} "),
                    }
                    update_confirmation = false;
                    settings_open = false;
                }
                KeyCode::Enter if update_confirmation => {
                    break Ok(Some(SessionChoice::InstallUpdate));
                }
                _ if update_confirmation => {}
                KeyCode::Esc if settings_open => settings_open = false,
                KeyCode::Char('q') if settings_open => settings_open = false,
                KeyCode::Up | KeyCode::Char('k') if settings_open => {
                    selected_setting = selected_setting.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') if settings_open => {
                    selected_setting = (selected_setting + 1).min(2)
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ')
                    if settings_open && !update_confirmation =>
                {
                    if selected_setting == 0 {
                        sound_enabled = !sound_enabled;
                        match set_sound_preference(state_dir, sound_enabled) {
                            Ok(()) => {
                                hint = format!(
                                    " Notification sounds: {} ",
                                    if sound_enabled { "ON" } else { "OFF" }
                                )
                            }
                            Err(error) => hint = format!(" Sound setting failed: {error:#} "),
                        }
                    } else if selected_setting == 1 {
                        hint = match cycle_theme(state_dir) {
                            Ok(name) => format!(" Theme: {name} "),
                            Err(error) => format!(" Theme failed: {error:#} "),
                        };
                    } else if available_update.is_some() {
                        update_confirmation = true;
                    } else {
                        hint = " Bastion is up to date ".to_owned();
                    }
                }
                KeyCode::Esc if removing => removing = false,
                KeyCode::Enter if removing => {
                    let root = workspace_root_at(&workspaces, selected)?;
                    match remove_workspace_request(
                        state_dir,
                        root,
                        workspace_pane_count(&workspaces[selected]) > 0,
                    ) {
                        Ok(()) => {
                            workspaces = listed_workspaces(state_dir)?;
                            if workspaces.is_empty() {
                                break Ok(None);
                            }
                            selected = selected.min(workspaces.len() - 1);
                            hint = " Workspace removed · select another workspace ".to_owned();
                        }
                        Err(error) => hint = format!(" Remove failed: {error:#} "),
                    }
                    removing = false;
                }
                KeyCode::Char('q') | KeyCode::Esc if !settings_open => break Ok(None),
                KeyCode::Up | KeyCode::Char('k') if !settings_open => {
                    selected = selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') if !settings_open => {
                    selected = (selected + 1).min(workspaces.len() - 1)
                }
                KeyCode::Char('t') if !settings_open && !removing => {
                    hint = match cycle_theme(state_dir) {
                        Ok(name) => {
                            format!(" Theme: {name} · press t to cycle · Enter opens workspace ")
                        }
                        Err(error) => format!(" Theme failed: {error:#} "),
                    }
                }
                KeyCode::Char('d') if !settings_open => removing = true,
                KeyCode::Char('s') if !removing => {
                    settings_open = true;
                    selected_setting = 0;
                    sound_enabled = Preferences::load(state_dir)?.notifications.sound;
                }
                KeyCode::Enter if !settings_open => {
                    let root = workspaces[selected]
                        .get("canonical_root")
                        .and_then(serde_json::Value::as_str)
                        .context("workspace has no path")?;
                    break Ok(Some(SessionChoice::Workspace(PathBuf::from(root))));
                }
                _ => {}
            },
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && removing =>
            {
                let size = terminal.size()?;
                let dialog = centered_rect(90, 55, Rect::new(0, 0, size.width, size.height));
                if contains(dialog, mouse.column, mouse.row) {
                    if mouse.column < dialog.x.saturating_add(dialog.width / 2) {
                        removing = false;
                    } else {
                        let root = workspace_root_at(&workspaces, selected)?;
                        match remove_workspace_request(
                            state_dir,
                            root,
                            workspace_pane_count(&workspaces[selected]) > 0,
                        ) {
                            Ok(()) => {
                                workspaces = listed_workspaces(state_dir)?;
                                if workspaces.is_empty() {
                                    break Ok(None);
                                }
                                selected = selected.min(workspaces.len() - 1);
                                hint = " Workspace removed · select another workspace ".to_owned();
                            }
                            Err(error) => hint = format!(" Remove failed: {error:#} "),
                        }
                        removing = false;
                    }
                } else {
                    removing = false;
                }
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && update_confirmation =>
            {
                let size = terminal.size()?;
                let dialog = centered_fixed(54, 10, Rect::new(0, 0, size.width, size.height));
                if !contains(dialog, mouse.column, mouse.row) {
                    update_confirmation = false;
                } else if mouse.row >= dialog.y.saturating_add(dialog.height.saturating_sub(3)) {
                    let relative = mouse.column.saturating_sub(dialog.x);
                    let region = u32::from(relative) * 3 / u32::from(dialog.width.max(1));
                    match region {
                        0 => update_confirmation = false,
                        1 => {
                            match skip_cached_update(state_dir) {
                                Ok(()) => {
                                    hint = " Update skipped until a newer release ".to_owned()
                                }
                                Err(error) => hint = format!(" Could not skip update: {error:#} "),
                            }
                            update_confirmation = false;
                            settings_open = false;
                        }
                        _ => break Ok(Some(SessionChoice::InstallUpdate)),
                    }
                }
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && settings_open =>
            {
                let size = terminal.size()?;
                let dialog = centered_fixed(50, 8, Rect::new(0, 0, size.width, size.height));
                if !contains(dialog, mouse.column, mouse.row) {
                    settings_open = false;
                } else if mouse.row == dialog.y.saturating_add(1) {
                    selected_setting = 0;
                    sound_enabled = !sound_enabled;
                    if let Err(error) = set_sound_preference(state_dir, sound_enabled) {
                        hint = format!(" Sound setting failed: {error:#} ");
                    }
                } else if mouse.row == dialog.y.saturating_add(2) {
                    selected_setting = 1;
                    if let Err(error) = cycle_theme(state_dir) {
                        hint = format!(" Theme failed: {error:#} ");
                    }
                } else if mouse.row == dialog.y.saturating_add(3) {
                    selected_setting = 2;
                    if available_update.is_some() {
                        update_confirmation = true;
                    } else {
                        hint = " Bastion is up to date ".to_owned();
                    }
                } else if mouse.row >= dialog.y.saturating_add(5) {
                    settings_open = false;
                }
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && !removing
                    && mouse.row >= 11 =>
            {
                let size = terminal.size()?;
                if mouse.row >= size.height.saturating_sub(2) {
                    let region = u32::from(mouse.column) * 3 / u32::from(size.width.max(1));
                    match region {
                        0 => {
                            settings_open = true;
                            selected_setting = 0;
                            sound_enabled = Preferences::load(state_dir)?.notifications.sound;
                        }
                        1 => removing = true,
                        _ => break Ok(None),
                    }
                    continue;
                }
                // Row 10 is the workspace-list border; rows from 11 carry
                // list items below the MOTD-style masthead.
                let index = usize::from(mouse.row.saturating_sub(11));
                if index < workspaces.len() {
                    let root = workspaces[index]
                        .get("canonical_root")
                        .and_then(serde_json::Value::as_str)
                        .context("workspace has no path")?;
                    break Ok(Some(SessionChoice::Workspace(PathBuf::from(root))));
                }
            }
            _ => {}
        }
    };
    result
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CachedUpdate {
    release: Option<CachedRelease>,
    skipped_version: Option<String>,
}

#[derive(Deserialize)]
struct CachedRelease {
    version: String,
}

fn cached_update_version(state_dir: &std::path::Path) -> Option<String> {
    let cache: CachedUpdate =
        serde_json::from_str(&std::fs::read_to_string(state_dir.join("update.json")).ok()?).ok()?;
    let release = cache.release?;
    if cache.skipped_version.as_deref() == Some(release.version.as_str()) {
        return None;
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let latest = Version::parse(&release.version).ok()?;
    (latest > current).then_some(release.version)
}

fn skip_cached_update(state_dir: &std::path::Path) -> Result<()> {
    let path = state_dir.join("update.json");
    let mut cache: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).context("read update cache")?)?;
    let version = cache
        .get("release")
        .and_then(|release| release.get("version"))
        .and_then(serde_json::Value::as_str)
        .context("no cached update is available")?
        .to_owned();
    cache["skipped_version"] = serde_json::Value::String(version);
    let temporary = state_dir.join("update.json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&cache)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn workspace_hint(update: Option<&str>) -> String {
    update.map_or_else(
        || " S SETTINGS     D REMOVE     Q QUIT ".to_owned(),
        |version| format!(" UPDATE v{version} AVAILABLE · S SETTINGS "),
    )
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

fn set_sound_preference(state_dir: &std::path::Path, enabled: bool) -> Result<()> {
    let mut preferences = Preferences::load(state_dir)?;
    preferences.notifications.sound = enabled;
    preferences.save(state_dir)
}

fn workspace_pane_count(workspace: &serde_json::Value) -> u64 {
    workspace
        .get("pane_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn workspace_root_at(workspaces: &[serde_json::Value], selected: usize) -> Result<PathBuf> {
    let root = workspaces
        .get(selected)
        .and_then(|workspace| workspace.get("canonical_root"))
        .and_then(serde_json::Value::as_str)
        .context("workspace has no path")?;
    Ok(PathBuf::from(root))
}

fn listed_workspaces(state_dir: &PathBuf) -> Result<Vec<serde_json::Value>> {
    Ok(dashboard_request(state_dir, Request::ListWorkspaces)?
        .get("workspaces")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn remove_workspace_request(state_dir: &PathBuf, cwd: PathBuf, force: bool) -> Result<()> {
    let response = dashboard_request(state_dir, Request::RemoveWorkspace { cwd, force })?;
    if response.get("type").and_then(serde_json::Value::as_str) == Some("error") {
        anyhow::bail!(
            response
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("workspace removal failed")
                .to_owned()
        )
    }
    Ok(())
}

/// Android's terminal column count varies dramatically with the selected
/// Termux font and display scale.  A 70–80 column phone is still portrait in
/// practice, so use a wider threshold only when running under Termux.
fn compact_layout(width: u16) -> bool {
    width <= 55
        || (std::env::var("PREFIX").is_ok_and(|prefix| prefix.contains("com.termux"))
            && width <= 85)
}

enum DashboardExit {
    Quit,
    Attach {
        pane_id: String,
        label: String,
        preview: Option<PanePreview>,
    },
}

/// A preview deliberately reads bounded daemon history instead of opening a
/// second PTY attachment. Two attachments resize the same PTY, which would
/// disrupt an inactive agent's full-screen UI.
#[derive(Clone)]
struct PanePreview {
    pane_id: String,
    label: String,
}

enum EmbeddedPaneExit {
    Workspace,
    Reattach,
    Focus(String),
}

#[derive(Clone, Copy, Default)]
struct PaneInputModes {
    application_cursor_keys: bool,
    bracketed_paste: bool,
}

enum PaneInputEvent {
    Reattach,
    Mouse(MouseEvent),
    Escape,
    Workspace,
    FocusPreview,
    ReturnToLiveBottom,
    Offline,
}

/// Reads Android keyboard events independently of painting the Ratatui grid.
/// The thread stops promptly through its short `poll` timeout when the pane
/// closes, so it cannot consume keys intended for the workspace dashboard.
struct PaneInputPump {
    stopped: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for PaneInputPump {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn start_pane_input_pump(
    writer: Arc<Mutex<UnixStream>>,
    modes: Arc<Mutex<PaneInputModes>>,
) -> (PaneInputPump, mpsc::Receiver<PaneInputEvent>) {
    let (sender, receiver) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::clone(&stopped);
    let join = thread::spawn(move || {
        while !stop_signal.load(Ordering::Acquire) {
            if !poll(Duration::from_millis(20)).unwrap_or(false) {
                continue;
            }
            let Ok(event) = read() else {
                break;
            };
            let event_to_send = match event {
                Event::Resize(_, _) => Some(PaneInputEvent::Reattach),
                Event::Mouse(mouse) => Some(PaneInputEvent::Mouse(mouse)),
                Event::Paste(text) => {
                    let bracketed = modes.lock().unwrap().bracketed_paste;
                    let bytes = bracketed_paste_bytes(&text, bracketed);
                    if writer.lock().unwrap().write_all(&bytes).is_err() {
                        Some(PaneInputEvent::Offline)
                    } else {
                        Some(PaneInputEvent::ReturnToLiveBottom)
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('b'))
                    {
                        Some(PaneInputEvent::Workspace)
                    } else if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Tab)
                    {
                        Some(PaneInputEvent::FocusPreview)
                    } else {
                        let modes = *modes.lock().unwrap();
                        let bytes =
                            key_to_bytes(key.code, key.modifiers, modes.application_cursor_keys);
                        if let Some(bytes) = bytes {
                            if writer.lock().unwrap().write_all(&bytes).is_err() {
                                Some(PaneInputEvent::Offline)
                            } else if key.code == KeyCode::Esc {
                                Some(PaneInputEvent::Escape)
                            } else {
                                Some(PaneInputEvent::ReturnToLiveBottom)
                            }
                        } else {
                            None
                        }
                    }
                }
                _ => None,
            };
            if let Some(event) = event_to_send
                && sender.send(event).is_err()
            {
                break;
            }
        }
    });
    (
        PaneInputPump {
            stopped,
            join: Some(join),
        },
        receiver,
    )
}

fn embedded_pane(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state_dir: &PathBuf,
    pane_id: &str,
    pane_label: &str,
    preview: Option<&PanePreview>,
) -> Result<EmbeddedPaneExit> {
    let screen = terminal.size()?;
    let portrait = compact_layout(screen.width);
    // A background pane preview requires synchronous snapshot RPC and a
    // second terminal render. Pause it while attached: live input latency is
    // more important than passive chrome.
    let preview_rows = 0;
    // The terminal viewport has left/right borders, so the PTY must be two
    // columns narrower than the outer screen or its last cells will wrap.
    let cols = screen.width.saturating_sub(2).max(20);
    let header_rows = if portrait { 1 } else { 3 };
    let rows = screen
        .height
        .saturating_sub(header_rows)
        .saturating_sub(preview_rows)
        .max(4);
    let mut stream = UnixStream::connect(state_dir.join("workspace.sock"))?;
    serde_json::to_writer(
        &mut stream,
        &Request::Attach {
            pane_id: pane_id.to_owned(),
            cols,
            rows,
        },
    )?;
    stream.write_all(b"\n")?;
    let response: serde_json::Value = serde_json::from_slice(&read_line(&mut stream)?)?;
    if let Some(message) = response.get("message").and_then(serde_json::Value::as_str) {
        anyhow::bail!(message.to_owned());
    }

    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let input_modes = Arc::new(Mutex::new(PaneInputModes::default()));
    let (_input_pump, input_rx) =
        start_pane_input_pump(Arc::clone(&writer), Arc::clone(&input_modes));

    let (output_tx, output_rx) = mpsc::channel();
    let mut reader = stream.try_clone()?;
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || output_tx.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut terminal_grid = workspace_terminal::Terminal::new(rows, cols);
    let mut offline = false;
    let mut needs_draw = true;
    let mut last_draw = Instant::now() - Duration::from_millis(34);
    let mut prior_screen: Option<workspace_terminal::Snapshot> = None;
    let mut retained_frame: Option<Buffer> = None;
    loop {
        loop {
            match output_rx.try_recv() {
                Ok(data) => {
                    terminal_grid.process(&data);
                    *input_modes.lock().unwrap() = PaneInputModes {
                        application_cursor_keys: terminal_grid.application_cursor_keys(),
                        bracketed_paste: terminal_grid.bracketed_paste(),
                    };
                    for reply in terminal_grid.take_replies() {
                        let _ = writer.lock().unwrap().write_all(&reply);
                    }
                    needs_draw = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    offline = true;
                    needs_draw = true;
                    break;
                }
            }
        }
        if needs_draw && last_draw.elapsed() >= Duration::from_millis(33) {
            let active_screen = terminal_grid.snapshot();
            let dirty_rows = changed_terminal_rows(prior_screen.as_ref(), &active_screen);
            let full_terminal_draw = retained_frame.is_none();
            let completed = terminal.draw(|frame| {
                if let Some(previous) = retained_frame.as_ref() {
                    *frame.buffer_mut() = previous.clone();
                }
                let area = frame.area();
                let mut constraints = vec![Constraint::Length(header_rows), Constraint::Min(4)];
                if preview_rows > 0 {
                    constraints.push(Constraint::Length(preview_rows));
                }
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(constraints)
                    .split(area);
                let header = if portrait {
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            "● ",
                            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            pane_label.to_owned(),
                            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                        ),
                    ]))
                } else {
                    Paragraph::new(vec![
                        Line::from(vec![
                            Span::styled(
                                " ● ACTIVE ",
                                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("  {pane_label}"),
                                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(Span::styled(
                            if offline {
                                "   pane offline · tap to return".to_owned()
                            } else {
                                format!(
                                    "   pane {} · input routed here",
                                    &pane_id[..pane_id.len().min(8)]
                                )
                            },
                            Style::default().fg(if offline { error() } else { muted() }),
                        )),
                    ])
                    .block(Block::default().borders(Borders::BOTTOM))
                };
                frame.render_widget(header, chunks[0]);
                let pane_block = Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT)
                    .border_style(Style::default().fg(border()));
                let pane_inner = pane_block.inner(chunks[1]);
                frame.render_widget(pane_block, chunks[1]);
                render_terminal_grid(
                    frame,
                    pane_inner,
                    &active_screen,
                    if full_terminal_draw {
                        None
                    } else {
                        Some(&dirty_rows)
                    },
                );
            })?;
            retained_frame = Some(completed.buffer.clone());
            prior_screen = Some(active_screen);
            needs_draw = false;
            last_draw = Instant::now();
        }

        let wait = if needs_draw {
            Duration::from_millis(4)
        } else {
            Duration::from_millis(12)
        };
        let Ok(event) = input_rx.recv_timeout(wait) else {
            continue;
        };
        match event {
            PaneInputEvent::Reattach => return Ok(EmbeddedPaneExit::Reattach),
            PaneInputEvent::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) && mouse.column >= 1
                    && mouse.column < screen.width.saturating_sub(1)
                    && mouse.row >= header_rows
                    && mouse.row < header_rows.saturating_add(rows) =>
            {
                // A terminal viewport should scroll locally first. This is
                // the familiar Termux behavior and does not require every
                // agent TUI to implement touch/mouse scrolling itself.
                let x = mouse.column.saturating_sub(1).min(cols.saturating_sub(1)) + 1;
                let y = mouse
                    .row
                    .saturating_sub(header_rows)
                    .min(rows.saturating_sub(1))
                    + 1;
                let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                if terminal_grid.scroll_viewport(if up { 3 } else { -3 }) {
                    needs_draw = true;
                    continue;
                }
                // At the end of Bastion's own scrollback, let an application
                // that requested terminal mouse input handle the gesture.
                let bytes = pane_scroll_bytes(terminal_grid.mouse_protocol(), up, x, y);
                if writer.lock().unwrap().write_all(&bytes).is_err() {
                    offline = true;
                    needs_draw = true;
                }
            }
            PaneInputEvent::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && (offline || mouse.row < header_rows) =>
            {
                return Ok(EmbeddedPaneExit::Workspace);
            }
            PaneInputEvent::Mouse(mouse)
                if preview_rows > 0
                    && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && mouse.row >= screen.height.saturating_sub(preview_rows) =>
            {
                if let Some(preview) = preview {
                    return Ok(EmbeddedPaneExit::Focus(preview.pane_id.clone()));
                }
            }
            PaneInputEvent::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && mouse.column >= 1
                    && mouse.column < screen.width.saturating_sub(1)
                    && mouse.row >= header_rows
                    && mouse.row < header_rows.saturating_add(rows) =>
            {
                // Claude/Codex fullscreen TUIs opt into terminal mouse input
                // for caret placement. Bastion owns its header, but content
                // taps belong to the PTY at its local 1-based cell position.
                let x = mouse.column.saturating_sub(1).min(cols.saturating_sub(1)) + 1;
                let y = mouse
                    .row
                    .saturating_sub(header_rows)
                    .min(rows.saturating_sub(1))
                    + 1;
                if let Some(bytes) = pane_click_bytes(terminal_grid.mouse_protocol(), x, y)
                    && writer.lock().unwrap().write_all(&bytes).is_err()
                {
                    offline = true;
                    needs_draw = true;
                }
            }
            PaneInputEvent::ReturnToLiveBottom => {
                terminal_grid.scroll_viewport_to_bottom();
                needs_draw = true;
            }
            PaneInputEvent::Escape => {
                if offline {
                    return Ok(EmbeddedPaneExit::Workspace);
                }
            }
            PaneInputEvent::Workspace => return Ok(EmbeddedPaneExit::Workspace),
            PaneInputEvent::FocusPreview => {
                if let Some(preview) = preview {
                    return Ok(EmbeddedPaneExit::Focus(preview.pane_id.clone()));
                }
            }
            PaneInputEvent::Offline => {
                offline = true;
                needs_draw = true;
            }
            PaneInputEvent::Mouse(_) => {}
        }
    }
}

fn bracketed_paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len().saturating_add(12));
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    bytes.extend_from_slice(text.as_bytes());
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes
}

/// Convert an Android touch-scroll gesture into terminal input.  Applications
/// that explicitly requested mouse reporting receive their chosen protocol;
/// conventional TUIs receive Shift+PageUp/PageDown instead.
fn pane_scroll_bytes(protocol: Option<MouseProtocol>, up: bool, x: u16, y: u16) -> Vec<u8> {
    let button = if up { 64 } else { 65 };
    match protocol {
        Some(MouseProtocol::Sgr) => format!("\x1b[<{button};{x};{y}M").into_bytes(),
        Some(MouseProtocol::X10) => vec![
            b'\x1b',
            b'[',
            b'M',
            (32 + button) as u8,
            (32 + x.min(223)) as u8,
            (32 + y.min(223)) as u8,
        ],
        None if up => b"\x1b[5~".to_vec(),
        None => b"\x1b[6~".to_vec(),
    }
}

/// A touchscreen has no separate mouse-up event in some Termux/Android
/// combinations, so emit a complete click pair on the initial touch-down.
fn pane_click_bytes(protocol: Option<MouseProtocol>, x: u16, y: u16) -> Option<Vec<u8>> {
    match protocol {
        // SGR keeps the button code on release; the lowercase `m` is the
        // release marker. Using X10's synthetic button 3 here makes some
        // fullscreen TUIs discard the click.
        Some(MouseProtocol::Sgr) => Some(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").into_bytes()),
        Some(MouseProtocol::X10) => Some(vec![
            b'\x1b',
            b'[',
            b'M',
            32,
            (32 + x.min(223)) as u8,
            (32 + y.min(223)) as u8,
            b'\x1b',
            b'[',
            b'M',
            35,
            (32 + x.min(223)) as u8,
            (32 + y.min(223)) as u8,
        ]),
        None => None,
    }
}

/// Draw terminal cells at their exact coordinates. `Paragraph` is a document
/// renderer, so it is allowed to wrap/reflow spans; a terminal grid is not.
/// This path deliberately writes into Ratatui's backing buffer one cell at a
/// time, which keeps Claude/Codex frames, popup edges, and spacing intact.
fn render_terminal_grid(
    frame: &mut ratatui::Frame,
    area: Rect,
    screen: &workspace_terminal::Snapshot,
    dirty_rows: Option<&[usize]>,
) {
    let start = screen.cells.len().saturating_sub(usize::from(area.height));
    let rows = dirty_rows
        .map(|rows| rows.to_vec())
        .unwrap_or_else(|| (start..screen.cells.len()).collect());
    for source_row in rows {
        if source_row < start {
            continue;
        }
        let target_row = source_row - start;
        if target_row >= usize::from(area.height) {
            break;
        }
        let row = &screen.cells[source_row];
        for (col, cell) in row.iter().take(usize::from(area.width)).enumerate() {
            if cell.wide_continuation {
                continue;
            }
            let mut style = Style::default()
                .fg(terminal_color(&cell.foreground))
                .bg(terminal_color(&cell.background));
            if cell.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if let Some(target) = frame.buffer_mut().cell_mut((
                area.x.saturating_add(col as u16),
                area.y.saturating_add(target_row as u16),
            )) {
                target.set_symbol(&cell.text).set_style(style);
            }
        }
    }
    if let Some(cursor) = screen.cursor
        && usize::from(cursor.row) >= start
        && usize::from(cursor.row) < start + usize::from(area.height)
        && cursor.column < area.width
    {
        let row = cursor.row as usize - start;
        if let Some(target) = frame.buffer_mut().cell_mut((
            area.x.saturating_add(cursor.column),
            area.y.saturating_add(row as u16),
        )) {
            // A bright inverted cell remains legible across Bastion themes
            // and mirrors Termux's familiar block cursor. The agent-provided
            // cursor shape is retained in the snapshot for future bar/line
            // rendering, while a visible block is the reliable mobile default.
            target.set_style(Style::default().fg(Color::Black).bg(Color::White));
        }
    }
}

/// Return only terminal rows whose cells (or cursor decoration) changed.
/// Ratatui begins each frame with a blank buffer, so the caller seeds it from
/// the previous frame before asking this renderer to patch these rows.
fn changed_terminal_rows(
    previous: Option<&workspace_terminal::Snapshot>,
    current: &workspace_terminal::Snapshot,
) -> Vec<usize> {
    let Some(previous) = previous else {
        return (0..current.cells.len()).collect();
    };
    if previous.rows != current.rows || previous.cols != current.cols {
        return (0..current.cells.len()).collect();
    }
    let mut rows = previous
        .cells
        .iter()
        .zip(&current.cells)
        .enumerate()
        .filter_map(|(row, (before, after))| (before != after).then_some(row))
        .collect::<Vec<_>>();
    for cursor in [previous.cursor, current.cursor].into_iter().flatten() {
        let row = usize::from(cursor.row);
        if !rows.contains(&row) {
            rows.push(row);
        }
    }
    rows.sort_unstable();
    rows
}

fn terminal_color(color: &workspace_terminal::CellColor) -> Color {
    use workspace_terminal::CellColor;
    match color {
        CellColor::DefaultForeground => Color::White,
        CellColor::DefaultBackground => Color::Black,
        CellColor::Rgb { r, g, b } => Color::Rgb(*r, *g, *b),
        CellColor::Indexed(index) => Color::Indexed(*index),
        // ANSI names 0..15 correspond exactly to the terminal palette.
        CellColor::Named(index) if *index < 16 => Color::Indexed(*index as u8),
        // Alacritty's Dim*/BrightForeground/DimForeground values.
        CellColor::Named(_) => Color::White,
    }
}

fn key_to_bytes(
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    application_cursor_keys: bool,
) -> Option<Vec<u8>> {
    match code {
        KeyCode::Char(character) if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
            character
                .is_ascii_alphabetic()
                .then(|| vec![(character.to_ascii_lowercase() as u8) - b'a' + 1])
        }
        KeyCode::Char(character) => Some(character.to_string().into_bytes()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(
            if application_cursor_keys {
                b"\x1bOA"
            } else {
                b"\x1b[A"
            }
            .to_vec(),
        ),
        KeyCode::Down => Some(
            if application_cursor_keys {
                b"\x1bOB"
            } else {
                b"\x1b[B"
            }
            .to_vec(),
        ),
        KeyCode::Right => Some(
            if application_cursor_keys {
                b"\x1bOC"
            } else {
                b"\x1b[C"
            }
            .to_vec(),
        ),
        KeyCode::Left => Some(
            if application_cursor_keys {
                b"\x1bOD"
            } else {
                b"\x1b[D"
            }
            .to_vec(),
        ),
        KeyCode::Home => Some(
            if application_cursor_keys {
                b"\x1bOH"
            } else {
                b"\x1b[H"
            }
            .to_vec(),
        ),
        KeyCode::End => Some(
            if application_cursor_keys {
                b"\x1bOF"
            } else {
                b"\x1b[F"
            }
            .to_vec(),
        ),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum DashboardAction {
    NewTab,
    NewShell,
    Resume,
    DeleteTab,
    StopPane,
    RenamePane,
}

struct DashboardInput {
    action: DashboardAction,
    value: String,
    target: Option<String>,
}

fn window_dashboard_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state_dir: &PathBuf,
    workspace: &PathBuf,
    remembered_tab: &mut usize,
    notice: &mut String,
) -> Result<DashboardExit> {
    let mut selected_tab = *remembered_tab;
    let mut selected_pane = 0_usize;
    let mut input: Option<DashboardInput> = None;
    let mut manage_open = false;
    let mut resume_picker_open = false;
    let mut selected_resume = 0_usize;
    loop {
        let status = dashboard_request(state_dir, Request::Status)?;
        let tabs = dashboard_request(
            state_dir,
            Request::ListTabs {
                cwd: Some(workspace.to_path_buf()),
            },
        )?;
        let slots = dashboard_request(
            state_dir,
            Request::ListSlots {
                cwd: Some(workspace.to_path_buf()),
            },
        )?;
        let tab_values = tabs
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if tab_values.is_empty() {
            anyhow::bail!("workspace has no tabs");
        }
        selected_tab = selected_tab.min(tab_values.len() - 1);
        let tab_name = tab_values[selected_tab]
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Main");
        let workspace_root = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.clone());
        let workspace_root = workspace_root.to_string_lossy();
        let panes: Vec<serde_json::Value> = status
            .get("pane_details")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter(|pane| {
                pane.get("workspace_root")
                    .and_then(serde_json::Value::as_str)
                    == Some(workspace_root.as_ref())
                    && pane.get("tab").and_then(serde_json::Value::as_str) == Some(tab_name)
            })
            .cloned()
            .collect();
        let resumable_slots = saved_session_slots(&slots);
        selected_pane = selected_pane.min(panes.len().saturating_sub(1));
        selected_resume = selected_resume.min(resumable_slots.len().saturating_sub(1));
        let screen_size = terminal.size()?;
        let screen = dashboard_rect(Rect::new(0, 0, screen_size.width, screen_size.height));
        let areas = window_areas(screen);
        terminal.draw(|frame| {
            draw_window_dashboard(
                frame,
                screen,
                &tab_values,
                selected_tab,
                &panes,
                selected_pane,
                &slots,
                notice.as_str(),
                input.as_ref(),
                manage_open,
                resume_picker_open,
                &resumable_slots,
                selected_resume,
                workspace,
            );
        })?;

        match read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(active_input) = input.as_mut() {
                    match key.code {
                        KeyCode::Esc => input = None,
                        KeyCode::Backspace => {
                            active_input.value.pop();
                        }
                        KeyCode::Enter => {
                            let completed = input.take().expect("input exists");
                            *notice = match execute_dashboard_action(
                                completed.action,
                                &completed.value,
                                completed.target.as_deref(),
                                state_dir,
                                workspace,
                                tab_name,
                            ) {
                                Ok(message) => message,
                                Err(error) => format!("action failed: {error:#}"),
                            };
                        }
                        KeyCode::Char(character) => active_input.value.push(character),
                        _ => {}
                    }
                    continue;
                }
                if resume_picker_open {
                    match key.code {
                        KeyCode::Esc => resume_picker_open = false,
                        KeyCode::Up | KeyCode::Char('k') => {
                            selected_resume = selected_resume.saturating_sub(1)
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            selected_resume =
                                (selected_resume + 1).min(resumable_slots.len().saturating_sub(1));
                        }
                        KeyCode::Enter if !resumable_slots.is_empty() => {
                            *notice = resume_saved_slot(
                                &resumable_slots[selected_resume],
                                state_dir,
                                workspace,
                                tab_name,
                            );
                            resume_picker_open = false;
                        }
                        _ => {}
                    }
                    continue;
                }
                if manage_open {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('b') => manage_open = false,
                        KeyCode::Char('r') if !panes.is_empty() => {
                            input = selected_pane_input(
                                DashboardAction::RenamePane,
                                &panes[selected_pane],
                            );
                            manage_open = false;
                        }
                        KeyCode::Char('d') if !panes.is_empty() => {
                            input = selected_pane_input(
                                DashboardAction::StopPane,
                                &panes[selected_pane],
                            );
                            manage_open = false;
                        }
                        KeyCode::Char('s') if !panes.is_empty() => {
                            *notice = pane_status(&panes[selected_pane]);
                            manage_open = false;
                        }
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(DashboardExit::Quit),
                    KeyCode::Left | KeyCode::Char('h') => {
                        selected_tab = selected_tab.saturating_sub(1);
                        *remembered_tab = selected_tab;
                        selected_pane = 0;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        selected_tab = (selected_tab + 1).min(tab_values.len() - 1);
                        *remembered_tab = selected_tab;
                        selected_pane = 0;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected_pane = selected_pane.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected_pane = (selected_pane + 1).min(panes.len().saturating_sub(1));
                    }
                    KeyCode::Enter if !panes.is_empty() => {
                        let pane_id = panes[selected_pane]
                            .get("pane_id")
                            .and_then(serde_json::Value::as_str)
                            .context("pane has no ID")?;
                        return Ok(DashboardExit::Attach {
                            pane_id: pane_id.to_owned(),
                            label: panes[selected_pane]
                                .get("label")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("Pane")
                                .to_owned(),
                            preview: preview_for(&panes, pane_id),
                        });
                    }
                    KeyCode::Char('n') => {
                        input = Some(new_dashboard_input(DashboardAction::NewTab))
                    }
                    KeyCode::Char('x') => {
                        input = Some(new_dashboard_input(DashboardAction::DeleteTab))
                    }
                    KeyCode::Char('m') if !panes.is_empty() => manage_open = true,
                    KeyCode::Char('r') => {
                        if resumable_slots.is_empty() {
                            *notice = "no saved agent sessions in this workspace".to_owned();
                        } else {
                            resume_picker_open = true;
                        }
                    }
                    KeyCode::Char('t') => {
                        *notice = match cycle_theme(state_dir) {
                            Ok(name) => format!("theme: {name}"),
                            Err(error) => format!("theme failed: {error:#}"),
                        };
                    }
                    KeyCode::Char('s') => {
                        *notice = match execute_dashboard_action(
                            DashboardAction::NewShell,
                            "",
                            None,
                            state_dir,
                            workspace,
                            tab_name,
                        ) {
                            Ok(message) => message,
                            Err(error) => format!("action failed: {error:#}"),
                        };
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
            {
                if input.is_some() {
                    continue;
                }
                if resume_picker_open {
                    let menu = resume_menu_rect(screen);
                    let row = usize::from(mouse.row.saturating_sub(menu.y));
                    let slot_index = row.saturating_sub(3);
                    if row >= 3 && slot_index < resumable_slots.len() {
                        *notice = resume_saved_slot(
                            &resumable_slots[slot_index],
                            state_dir,
                            workspace,
                            tab_name,
                        );
                    }
                    resume_picker_open = false;
                    continue;
                }
                if manage_open {
                    let menu = pane_menu_rect(screen);
                    match mouse.row.saturating_sub(menu.y) {
                        4 if !panes.is_empty() => {
                            input = selected_pane_input(
                                DashboardAction::RenamePane,
                                &panes[selected_pane],
                            );
                            manage_open = false;
                        }
                        5 if !panes.is_empty() => {
                            input = selected_pane_input(
                                DashboardAction::StopPane,
                                &panes[selected_pane],
                            );
                            manage_open = false;
                        }
                        6 if !panes.is_empty() => {
                            *notice = pane_status(&panes[selected_pane]);
                            manage_open = false;
                        }
                        _ => manage_open = false,
                    }
                    continue;
                }
                if contains(areas.tabs, mouse.column, mouse.row) {
                    if compact_layout(screen.width) {
                        // Portrait has a deliberately compact tab strip:
                        // tap its left/right half to move through tabs.
                        if mouse.column < areas.tabs.x.saturating_add(areas.tabs.width / 2) {
                            selected_tab = selected_tab.saturating_sub(1);
                        } else {
                            selected_tab = (selected_tab + 1).min(tab_values.len() - 1);
                        }
                    } else {
                        let tab_width = areas.tabs.width.max(1);
                        let tapped_tab = (usize::from(mouse.column.saturating_sub(areas.tabs.x))
                            * tab_values.len()
                            / usize::from(tab_width))
                        .min(tab_values.len() - 1);
                        selected_tab = tapped_tab;
                    }
                    *remembered_tab = selected_tab;
                    selected_pane = 0;
                } else if contains(areas.panes, mouse.column, mouse.row) && !panes.is_empty() {
                    let index =
                        usize::from(mouse.row.saturating_sub(areas.panes.y.saturating_add(1)));
                    if index < panes.len() {
                        // A first tap changes the selection, enabling the
                        // touch actions below.  Tapping the selected pane
                        // again opens it, preserving a quick path into it.
                        if index != selected_pane {
                            selected_pane = index;
                            *notice = format!(
                                "selected {} · tap again to open",
                                panes[index]
                                    .get("label")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("Pane")
                            );
                            continue;
                        }
                        let pane_id = panes[index]
                            .get("pane_id")
                            .and_then(serde_json::Value::as_str)
                            .context("pane has no ID")?;
                        return Ok(DashboardExit::Attach {
                            pane_id: pane_id.to_owned(),
                            label: panes[index]
                                .get("label")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("Pane")
                                .to_owned(),
                            preview: preview_for(&panes, pane_id),
                        });
                    }
                } else if contains(areas.actions, mouse.column, mouse.row) {
                    if mouse.row != areas.actions.y.saturating_add(1) {
                        continue;
                    }
                    let action = usize::from(mouse.column.saturating_sub(areas.actions.x)) * 4
                        / usize::from(areas.actions.width.max(1));
                    match action {
                        0 => input = Some(new_dashboard_input(DashboardAction::NewTab)),
                        1 => {
                            *notice = match execute_dashboard_action(
                                DashboardAction::NewShell,
                                "",
                                None,
                                state_dir,
                                workspace,
                                tab_name,
                            ) {
                                Ok(message) => message,
                                Err(error) => format!("action failed: {error:#}"),
                            };
                        }
                        2 if resumable_slots.is_empty() => {
                            *notice = "no saved agent sessions in this workspace".to_owned()
                        }
                        2 => resume_picker_open = true,
                        _ if panes.is_empty() => {
                            *notice = "select or create a pane first".to_owned()
                        }
                        _ => manage_open = true,
                    }
                }
            }
            _ => {}
        }
    }
}

fn preview_for(panes: &[serde_json::Value], active_id: &str) -> Option<PanePreview> {
    panes
        .iter()
        .find(|pane| pane.get("pane_id").and_then(serde_json::Value::as_str) != Some(active_id))
        .and_then(|pane| {
            Some(PanePreview {
                pane_id: pane.get("pane_id")?.as_str()?.to_owned(),
                label: pane
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Pane")
                    .to_owned(),
            })
        })
}

struct WindowAreas {
    tabs: Rect,
    panes: Rect,
    slots: Rect,
    actions: Rect,
}

fn dashboard_rect(screen: Rect) -> Rect {
    let portrait = compact_layout(screen.width);
    let height = if portrait {
        screen.height.min(15)
    } else {
        screen.height.min(20)
    };
    Rect {
        x: screen.x,
        y: if portrait {
            screen.y
        } else {
            screen
                .y
                .saturating_add((screen.height.saturating_sub(height)) / 3)
        },
        width: screen.width,
        height,
    }
}

fn window_areas(area: Rect) -> WindowAreas {
    let portrait = compact_layout(area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(if portrait { 0 } else { 3 }),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    WindowAreas {
        tabs: chunks[0],
        panes: chunks[1],
        slots: chunks[2],
        actions: chunks[3],
    }
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn new_dashboard_input(action: DashboardAction) -> DashboardInput {
    DashboardInput {
        action,
        value: String::new(),
        target: None,
    }
}

fn selected_pane_input(
    action: DashboardAction,
    pane: &serde_json::Value,
) -> Option<DashboardInput> {
    let target = pane.get("pane_id")?.as_str()?.to_owned();
    Some(DashboardInput {
        action,
        value: if matches!(action, DashboardAction::RenamePane) {
            pane.get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Pane")
                .to_owned()
        } else {
            String::new()
        },
        target: Some(target),
    })
}

fn pane_status(pane: &serde_json::Value) -> String {
    let label = pane
        .get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Pane");
    let command = pane
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let health = pane
        .get("health")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let idle = pane
        .get("idle_seconds")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    format!("{label} · {health} · idle {idle}s · {command}")
}

fn pane_menu_rect(area: Rect) -> Rect {
    centered_rect(92, 88, area)
}

fn resume_menu_rect(area: Rect) -> Rect {
    centered_rect(92, 88, area)
}

fn saved_session_slots(slots: &serde_json::Value) -> Vec<serde_json::Value> {
    slots
        .get("slots")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|slot| {
            slot.get("restore_enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                && slot
                    .get("native_session_id")
                    .is_some_and(|session| !session.is_null())
                && slot.get("last_state").and_then(serde_json::Value::as_str) != Some("stopped")
        })
        .cloned()
        .collect()
}

fn resume_saved_slot(
    slot: &serde_json::Value,
    state_dir: &PathBuf,
    workspace: &PathBuf,
    tab: &str,
) -> String {
    let agent = slot
        .get("agent_kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let name = slot
        .get("slot_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if agent.is_empty() || name.is_empty() {
        return "saved session is incomplete".to_owned();
    }
    match execute_dashboard_action(
        DashboardAction::Resume,
        &format!("{agent} {name}"),
        None,
        state_dir,
        workspace,
        tab,
    ) {
        Ok(message) => message,
        Err(error) => format!("resume failed: {error:#}"),
    }
}

fn action_label(action: DashboardAction) -> &'static str {
    match action {
        DashboardAction::NewTab => "New tab name",
        DashboardAction::NewShell => "New shell",
        DashboardAction::Resume => "Resume: agent slot",
        DashboardAction::DeleteTab => "Close selected tab? Enter confirms · Esc cancels",
        DashboardAction::StopPane => "Stop selected pane? Enter confirms · Esc cancels",
        DashboardAction::RenamePane => "Pane name",
    }
}

fn execute_dashboard_action(
    action: DashboardAction,
    value: &str,
    target: Option<&str>,
    state_dir: &PathBuf,
    workspace: &PathBuf,
    tab: &str,
) -> Result<String> {
    let response = match action {
        DashboardAction::NewTab => dashboard_request(
            state_dir,
            Request::CreateTab {
                name: value.trim().to_owned(),
                cwd: Some(workspace.to_path_buf()),
            },
        )?,
        DashboardAction::NewShell => dashboard_request(
            state_dir,
            Request::StartShell {
                cwd: Some(workspace.to_path_buf()),
                tab: Some(tab.to_owned()),
            },
        )?,
        DashboardAction::Resume => {
            let mut parts = value.split_whitespace();
            let (Some(agent), Some(slot)) = (parts.next(), parts.next()) else {
                return Ok("Use: claude primary".to_owned());
            };
            dashboard_request(
                state_dir,
                Request::ResumeAgent {
                    agent: agent.to_owned(),
                    slot: slot.to_owned(),
                    cwd: Some(workspace.to_path_buf()),
                    tab: Some(tab.to_owned()),
                },
            )?
        }
        DashboardAction::DeleteTab => dashboard_request(
            state_dir,
            Request::DeleteTab {
                name: tab.to_owned(),
                cwd: Some(workspace.to_path_buf()),
            },
        )?,
        DashboardAction::StopPane => {
            let pane_id = target.context("no pane is selected")?;
            dashboard_request(
                state_dir,
                Request::StopPane {
                    pane_id: pane_id.to_owned(),
                },
            )?
        }
        DashboardAction::RenamePane => {
            let pane_id = target.context("no pane is selected")?;
            dashboard_request(
                state_dir,
                Request::RenamePane {
                    pane_id: pane_id.to_owned(),
                    name: value.trim().to_owned(),
                },
            )?
        }
    };
    Ok(compact_json_message(&response, "done"))
}

fn draw_window_dashboard(
    frame: &mut ratatui::Frame,
    area: Rect,
    tabs: &[serde_json::Value],
    selected_tab: usize,
    panes: &[serde_json::Value],
    selected_pane: usize,
    slots: &serde_json::Value,
    notice: &str,
    input: Option<&DashboardInput>,
    manage_open: bool,
    resume_picker_open: bool,
    resumable_slots: &[serde_json::Value],
    selected_resume: usize,
    workspace: &PathBuf,
) {
    let areas = window_areas(area);
    let portrait = compact_layout(area.width);
    let project = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let tab_spans = if portrait {
        let name = tabs
            .get(selected_tab)
            .and_then(|tab| tab.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Main");
        vec![
            Span::styled(" ‹ ", Style::default().fg(muted())),
            Span::styled(
                format!("{name}  ({}/{})", selected_tab + 1, tabs.len()),
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  › ", Style::default().fg(muted())),
        ]
    } else {
        tabs.iter()
            .enumerate()
            .flat_map(|(index, tab)| {
                let name = tab
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let style = if index == selected_tab {
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(muted())
                };
                [Span::styled(format!(" {name} "), style), Span::raw(" ")]
            })
            .collect::<Vec<_>>()
    };
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border()))
                .title(format!(" {project} · tabs ")),
        ),
        areas.tabs,
    );

    let pane_items = if panes.is_empty() {
        vec![ListItem::new(
            "No panes in this tab — tap +Pane to start a shell",
        )]
    } else {
        panes
            .iter()
            .map(|pane| {
                let label = pane
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let command = pane
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let health = pane
                    .get("health")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("waiting");
                let agent = pane
                    .get("agent_kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("shell");
                let (marker, health_color) = if health == "active" {
                    (" ● ", success())
                } else {
                    (" ○ ", warning())
                };
                let mut line = vec![
                    Span::styled(marker, Style::default().fg(health_color)),
                    Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
                ];
                if !portrait {
                    line.push(Span::styled(
                        format!("  {agent} · {health} · {command}"),
                        Style::default().fg(muted()),
                    ));
                } else {
                    line.push(Span::styled(
                        format!(" · {agent} {health}"),
                        Style::default().fg(health_color),
                    ));
                }
                ListItem::new(Line::from(line))
            })
            .collect()
    };
    let pane_list = List::new(pane_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border()))
                .title(" PANES · tap select · tap again open "),
        )
        .highlight_style(
            Style::default()
                .bg(focus_background())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut pane_state = ListState::default();
    if !panes.is_empty() {
        pane_state.select(Some(selected_pane));
    }
    frame.render_stateful_widget(pane_list, areas.panes, &mut pane_state);

    let slot_items = slots
        .get("slots")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(usize::from(areas.slots.height.saturating_sub(2)))
        .map(|slot| {
            let agent = slot
                .get("agent_kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let name = slot
                .get("slot_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let saved = if slot
                .get("native_session_id")
                .is_some_and(|id| !id.is_null())
            {
                "resume"
            } else {
                "new"
            };
            ListItem::new(format!(" {agent}/{name} · {saved}"))
        })
        .collect::<Vec<_>>();
    if !portrait {
        frame.render_widget(
            List::new(slot_items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border()))
                    .title(" Saved agents "),
            ),
            areas.slots,
        );
    }
    frame.render_widget(
        Paragraph::new(" + TAB       + PANE       RESUME       MANAGE ")
            .alignment(ratatui::layout::Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border()))
                    .title(" Workspace controls "),
            ),
        areas.actions,
    );
    let status_area = Rect {
        x: area.x,
        y: areas.actions.y.saturating_add(areas.actions.height),
        width: area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(notice)
            .style(Style::default().fg(if notice.is_empty() {
                muted()
            } else {
                warning()
            }))
            .wrap(Wrap { trim: true }),
        status_area,
    );
    if let Some(input) = input {
        let popup = centered_rect(80, 30, area);
        frame.render_widget(WidgetClear, popup);
        frame.render_widget(
            Paragraph::new(format!("{}\n\n{}", action_label(input.action), input.value))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(accent()))
                        .title(" Input · Enter save · Esc cancel "),
                )
                .wrap(Wrap { trim: true }),
            popup,
        );
    } else if resume_picker_open {
        let menu = resume_menu_rect(area);
        frame.render_widget(WidgetClear, menu);
        let entries = resumable_slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let agent = slot
                    .get("agent_kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("agent");
                let name = slot
                    .get("slot_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("session");
                let marker = if index == selected_resume {
                    "› "
                } else {
                    "  "
                };
                Line::from(Span::styled(
                    format!("{marker}{agent} · {name}"),
                    if index == selected_resume {
                        Style::default().fg(accent()).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    },
                ))
            })
            .collect::<Vec<_>>();
        let mut lines = vec![
            Line::from(Span::styled(
                "Saved agent sessions",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Tap a session to resume it",
                Style::default().fg(muted()),
            )),
            Line::raw(""),
        ];
        lines.extend(entries);
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Esc · Back",
            Style::default().fg(muted()),
        )));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(accent()))
                    .title(" Resume session "),
            ),
            menu,
        );
    } else if manage_open {
        let label = panes
            .get(selected_pane)
            .and_then(|pane| pane.get("label"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("No pane selected");
        let menu = pane_menu_rect(area);
        frame.render_widget(WidgetClear, menu);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    label.to_owned(),
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "Choose an action",
                    Style::default().fg(muted()),
                )),
                Line::raw(""),
                Line::from("  RENAME"),
                Line::from("  CLOSE PANE"),
                Line::from("  STATUS"),
                Line::raw(""),
                Line::from(Span::styled("  BACK", Style::default().fg(muted()))),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(accent()))
                    .title(" Pane menu · tap an option "),
            ),
            menu,
        );
    }
}

/// The workspace selector uses the established Termux MOTD as fixed Bastion
/// branding. Workspace-specific details belong in the selectable list below.
fn workspace_masthead() -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(Span::styled(
            "  ▪ ▪ ▪                        B A S T I O N  ",
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  ▄█████▄    ", Style::default().fg(accent())),
            Span::styled(
                "╭─────────────────────────────╮",
                Style::default().fg(border()),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ▐██▀█▀██▌   ", Style::default().fg(accent())),
            Span::styled(
                "│  >_  CODE · BUILD · DEPLOY  │",
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ▀█▀ ▀█▀    ", Style::default().fg(accent())),
            Span::styled(
                "╰─────────────────────────────╯",
                Style::default().fg(border()),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "─ WORKSPACES ─────────────────────── TERMUX ──",
            Style::default().fg(border()),
        )),
        Line::from(Span::styled(
            "  Select a project to continue",
            Style::default().fg(Color::White),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border())),
    )
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(horizontal[1])[1]
}

fn centered_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

/*
fn dashboard_loop(state_dir: &PathBuf, workspace: &PathBuf) -> Result<()> {
    let mut selected_tab = 0_usize;
    let mut selected_pane = 0_usize;
    let mut notice = String::new();
    loop {
        let status = dashboard_request(state_dir, Request::Status)?;
        let tabs = dashboard_request(
            state_dir,
            Request::ListTabs {
                cwd: Some(workspace),
            },
        )?;
        let slots = dashboard_request(
            state_dir,
            Request::ListSlots {
                cwd: Some(workspace),
            },
        )?;
        let tab_values = tabs
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if tab_values.is_empty() {
            anyhow::bail!("workspace has no tabs");
        }
        selected_tab = selected_tab.min(tab_values.len() - 1);
        let tab_name = tab_values[selected_tab]
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Main");
        let panes: Vec<&serde_json::Value> = status
            .get("pane_details")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter(|pane| pane.get("tab").and_then(serde_json::Value::as_str) == Some(tab_name))
            .collect();
        selected_pane = selected_pane.min(panes.len().saturating_sub(1));
        let layout = render_dashboard(
            workspace,
            &tab_values,
            selected_tab,
            &panes,
            selected_pane,
            &slots,
            &notice,
        )?;
        notice.clear();

        let code = match read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => key.code,
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
            {
                if mouse.row == layout.tab_row {
                    selected_tab = (usize::from(mouse.column) * tab_values.len() / layout.width)
                        .min(tab_values.len() - 1);
                    selected_pane = 0;
                    continue;
                }
                if mouse.row >= layout.pane_start
                    && mouse.row < layout.pane_start + panes.len().min(4) as u16
                    && !panes.is_empty()
                {
                    let index = usize::from(mouse.row - layout.pane_start);
                    let pane_id = panes[index]
                        .get("pane_id")
                        .and_then(serde_json::Value::as_str)
                        .context("pane has no ID")?;
                    disable_raw_mode()?;
                    let mut stream = UnixStream::connect(state_dir.join("workspace.sock"))?;
                    return attach(&mut stream, pane_id);
                }
                if mouse.row == layout.action_row_one {
                    match usize::from(mouse.column) * 3 / layout.width {
                        0 => KeyCode::Char('n'),
                        1 => KeyCode::Char('c'),
                        _ => KeyCode::Char('o'),
                    }
                } else if mouse.row == layout.action_row_two {
                    match usize::from(mouse.column) * 3 / layout.width {
                        0 => KeyCode::Char('s'),
                        1 => KeyCode::Char('r'),
                        _ => KeyCode::Char('q'),
                    }
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Left | KeyCode::Char('h') => {
                selected_tab = selected_tab.saturating_sub(1);
                selected_pane = 0;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                selected_tab = (selected_tab + 1).min(tab_values.len() - 1);
                selected_pane = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => selected_pane = selected_pane.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected_pane = (selected_pane + 1).min(panes.len().saturating_sub(1));
            }
            KeyCode::Enter if !panes.is_empty() => {
                let pane_id = panes[selected_pane]
                    .get("pane_id")
                    .and_then(serde_json::Value::as_str)
                    .context("pane has no ID")?;
                disable_raw_mode()?;
                let mut stream = UnixStream::connect(state_dir.join("workspace.sock"))?;
                return attach(&mut stream, pane_id);
            }
            KeyCode::Char('n') => {
                if let Some(name) = dashboard_prompt("New tab name: ")? {
                    let response = dashboard_request(
                        state_dir,
                        Request::CreateTab {
                            name: &name,
                            cwd: Some(workspace),
                        },
                    )?;
                    notice = compact_json_message(&response, "tab created");
                }
            }
            KeyCode::Char('c') | KeyCode::Char('o') => {
                let agent = if code == KeyCode::Char('c') {
                    "claude"
                } else {
                    "codex"
                };
                if let Some(slot) = dashboard_prompt(&format!("{agent} slot: "))? {
                    let response = dashboard_request(
                        state_dir,
                        Request::StartAgent {
                            agent,
                            slot: &slot,
                            cwd: Some(workspace),
                            tab: Some(tab_name),
                        },
                    )?;
                    notice = compact_json_message(&response, "pane started");
                }
            }
            KeyCode::Char('s') => {
                let response = dashboard_request(
                    state_dir,
                    Request::StartShell {
                        cwd: Some(workspace),
                        tab: Some(tab_name),
                    },
                )?;
                notice = compact_json_message(&response, "shell started");
            }
            KeyCode::Char('r') => {
                if let Some(input) = dashboard_prompt("Resume: agent slot: ")? {
                    let mut parts = input.split_whitespace();
                    if let (Some(agent), Some(slot)) = (parts.next(), parts.next()) {
                        let response = dashboard_request(
                            state_dir,
                            Request::ResumeAgent {
                                agent,
                                slot,
                                cwd: Some(workspace),
                                tab: Some(tab_name),
                            },
                        )?;
                        notice = compact_json_message(&response, "pane resumed");
                    } else {
                        notice = "Use: claude primary".to_owned();
                    }
                }
            }
            _ => {}
        }
    }
}

*/

fn dashboard_request(state_dir: &PathBuf, request: Request) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(state_dir.join("workspace.sock"))?;
    request_value(&mut stream, request)
}

struct NotificationListener {
    stop: mpsc::Sender<()>,
    active_pane: Arc<Mutex<Option<String>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl NotificationListener {
    fn start(state_dir: PathBuf) -> Self {
        let active_pane = Arc::new(Mutex::new(None));
        let (stop, stop_signal) = mpsc::channel();
        let active = Arc::clone(&active_pane);
        let join = thread::spawn(move || {
            let mut cursor = notification_cursor(&state_dir, 0)
                .map(|item| item.0)
                .unwrap_or(0);
            loop {
                match stop_signal.recv_timeout(Duration::from_millis(350)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                let Ok((latest, notifications)) = notification_cursor(&state_dir, cursor) else {
                    continue;
                };
                if latest < cursor {
                    cursor = latest;
                    continue;
                }
                cursor = latest;
                let focused = active.lock().unwrap().clone();
                let sound_enabled = Preferences::load(&state_dir)
                    .map(|preferences| preferences.notifications.sound)
                    .unwrap_or(true);
                for notification in notifications {
                    if notification
                        .get("pane_id")
                        .and_then(serde_json::Value::as_str)
                        == focused.as_deref()
                    {
                        continue;
                    }
                    if !sound_enabled {
                        continue;
                    }
                    match notification
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                    {
                        Some("attention") => {
                            bastion_audio::play_detached(bastion_audio::AlertSound::Attention)
                        }
                        Some("done") => {
                            bastion_audio::play_detached(bastion_audio::AlertSound::Done)
                        }
                        _ => {}
                    }
                }
            }
        });
        Self {
            stop,
            active_pane,
            join: Some(join),
        }
    }

    fn set_active(&self, pane_id: Option<String>) {
        *self.active_pane.lock().unwrap() = pane_id;
    }
}

impl Drop for NotificationListener {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn notification_cursor(
    state_dir: &PathBuf,
    after_sequence: u64,
) -> Result<(u64, Vec<serde_json::Value>)> {
    let response = dashboard_request(state_dir, Request::ListNotifications { after_sequence })?;
    let latest = response
        .get("latest_sequence")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(after_sequence);
    let notifications = response
        .get("notifications")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((latest, notifications))
}

/*
fn dashboard_prompt(label: &str) -> Result<Option<String>> {
    disable_raw_mode()?;
    let mut stdout = std::io::stdout();
    write!(stdout, "\r\n{label}")?;
    stdout.flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    enable_raw_mode()?;
    let input = input.trim().to_owned();
    Ok((!input.is_empty()).then_some(input))
}

*/

fn compact_json_message(response: &serde_json::Value, fallback: &str) -> String {
    if let Some(message) = response.get("message").and_then(serde_json::Value::as_str) {
        return message.to_owned();
    }
    if let Some(pane_id) = response.get("pane_id").and_then(serde_json::Value::as_str) {
        return format!("{fallback}: {}", &pane_id[..pane_id.len().min(8)]);
    }
    fallback.to_owned()
}

/*
struct DashboardLayout {
    width: usize,
    tab_row: u16,
    pane_start: u16,
    action_row_one: u16,
    action_row_two: u16,
}

fn render_dashboard(
    workspace: &PathBuf,
    tabs: &[serde_json::Value],
    selected_tab: usize,
    panes: &[&serde_json::Value],
    selected_pane: usize,
    slots: &serde_json::Value,
    notice: &str,
) -> Result<DashboardLayout> {
    let (columns, _) = size()?;
    // Keep one spare terminal column: writing into the last cell can trigger
    // automatic wrapping on Termux and corrupt the next dashboard row.
    let width = usize::from(columns).saturating_sub(1).clamp(28, 48);
    let inner = width - 2;
    let mut stdout = std::io::stdout();
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    let mut rows = Vec::new();
    rows.push(format!(
        "╭{}╮",
        fit(
            &format!(
                " WORKSPACE · {} ",
                workspace
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(".")
            ),
            inner,
            '─'
        )
    ));
    rows.push(format!(
        "│{}│",
        fit(" ←/→ tab · ↑/↓ pane · Enter attach ", inner, ' ')
    ));
    let tab_line = tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let name = tab
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            if index == selected_tab {
                format!("[{}]", name)
            } else {
                name.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    rows.push(format!("├{}┤", fit(" TABS ", inner, '─')));
    rows.push(format!("│{}│", fit(&format!(" {tab_line}"), inner, ' ')));
    rows.push(format!("├{}┤", fit(" PANES ", inner, '─')));
    if panes.is_empty() {
        rows.push(format!("│{}│", fit("  (no panes in this tab)", inner, ' ')));
    }
    for (index, pane) in panes.iter().take(4).enumerate() {
        let marker = if index == selected_pane { '›' } else { ' ' };
        let command = pane
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let pane_id = pane
            .get("pane_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        rows.push(format!(
            "│{}│",
            fit(
                &format!(
                    " {marker} ● {command} · {}",
                    &pane_id[..pane_id.len().min(6)]
                ),
                inner,
                ' '
            )
        ));
    }
    rows.push(format!("├{}┤", fit(" SLOTS ", inner, '─')));
    for slot in slots
        .get("slots")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(3)
    {
        let agent = slot
            .get("agent_kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let name = slot
            .get("slot_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let state = if slot
            .get("native_session_id")
            .is_some_and(|id| !id.is_null())
        {
            "resume"
        } else {
            "new"
        };
        rows.push(format!(
            "│{}│",
            fit(&format!("  {agent}/{name} · {state}"), inner, ' ')
        ));
    }
    let pane_lines = panes.len().clamp(1, 4) as u16;
    let slot_lines = slots
        .get("slots")
        .and_then(serde_json::Value::as_array)
        .map_or(0, |items| items.len().min(3)) as u16;
    rows.push(format!(
        "├{}┤",
        fit(" n TAB      c CLAUDE      o CODEX ", inner, '─')
    ));
    rows.push(format!(
        "│{}│",
        fit(" s SHELL    r RESUME      q QUIT ", inner, ' ')
    ));
    rows.push(format!("│{}│", fit(&format!(" {notice}"), inner, ' ')));
    rows.push(format!("╰{}╯", "─".repeat(inner)));
    write!(stdout, "\r{}", rows.join("\r\n"))?;
    stdout.flush()?;
    Ok(DashboardLayout {
        width,
        tab_row: 3,
        pane_start: 5,
        action_row_one: 6 + pane_lines + slot_lines,
        action_row_two: 7 + pane_lines + slot_lines,
    })
}

fn fit(value: &str, width: usize, fill: char) -> String {
    let mut output: String = value.chars().take(width).collect();
    output.push_str(
        &fill
            .to_string()
            .repeat(width.saturating_sub(output.chars().count())),
    );
    output
}

*/

fn logs(stream: &mut UnixStream, pane_id: &str) -> Result<()> {
    serde_json::to_writer(
        &mut *stream,
        &Request::PaneLog {
            pane_id: pane_id.to_owned(),
        },
    )?;
    stream.write_all(b"\n")?;
    let response: serde_json::Value = serde_json::from_slice(&read_line(stream)?)?;
    if let Some(message) = response.get("message").and_then(serde_json::Value::as_str) {
        anyhow::bail!("{message}");
    }
    let exit_code = response
        .get("exit_code")
        .and_then(serde_json::Value::as_u64)
        .map(|code| code.to_string())
        .unwrap_or_else(|| "running".to_owned());
    println!("--- pane {pane_id} ({exit_code}) ---");
    print!(
        "{}",
        response
            .get("output")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    );
    Ok(())
}

fn request(stream: &mut UnixStream, request: Request) -> Result<()> {
    println!("{}", request_value(stream, request)?);
    Ok(())
}

fn remove_workspace_command(stream: &mut UnixStream, cwd: PathBuf, force: bool) -> Result<()> {
    let response = request_value(stream, Request::RemoveWorkspace { cwd, force })?;
    if let Some(message) = response.get("message").and_then(serde_json::Value::as_str) {
        if response.get("type").and_then(serde_json::Value::as_str) == Some("error") {
            anyhow::bail!(message.to_owned());
        }
        println!("{message}");
    } else {
        println!("{response}");
    }
    Ok(())
}

fn request_value(stream: &mut UnixStream, request: Request) -> Result<serde_json::Value> {
    serde_json::to_writer(&mut *stream, &request)?;
    stream.write_all(b"\n")?;
    Ok(serde_json::from_slice(&read_line(stream)?)?)
}

fn attach(stream: &mut UnixStream, pane_id: &str) -> Result<()> {
    let (cols, rows) = size()?;
    serde_json::to_writer(
        &mut *stream,
        &Request::Attach {
            pane_id: pane_id.to_owned(),
            cols,
            rows,
        },
    )?;
    stream.write_all(b"\n")?;
    let response = String::from_utf8_lossy(&read_line(stream)?).into_owned();
    if response.contains("error") {
        anyhow::bail!("{response}");
    }
    enable_raw_mode()?;
    let mut input = stream.try_clone()?;
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut byte = [0_u8; 1];
        while let Ok(size) = stdin.read(&mut byte) {
            if size == 0 {
                break;
            }
            // Ctrl+] belongs to the workspace client: detach without sending a
            // control byte to the process running inside the PTY.
            if byte[0] == 0x1d {
                let _ = input.shutdown(std::net::Shutdown::Write);
                break;
            }
            if input.write_all(&byte[..size]).is_err() {
                break;
            }
        }
    });
    let mut stdout = std::io::stdout();
    let mut buffer = [0_u8; 4096];
    let result = loop {
        let size = stream.read(&mut buffer)?;
        if size == 0 {
            break Ok(());
        }
        stdout.write_all(&buffer[..size])?;
        stdout.flush()?;
    };
    disable_raw_mode()?;
    if result.is_ok() {
        println!("\r\nDetached. The pane is still running.");
    }
    result
}

fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_rows_include_the_old_and_new_cursor_rows() {
        let mut terminal = workspace_terminal::Terminal::new(2, 8);
        terminal.process(b"one\r\ntwo");
        let before = terminal.snapshot();
        terminal.process(b"\x1b[A");
        let after = terminal.snapshot();
        assert_eq!(changed_terminal_rows(Some(&before), &after), vec![0, 1]);
    }

    #[test]
    fn pane_click_uses_the_requested_mouse_protocol() {
        assert_eq!(pane_click_bytes(None, 4, 7), None);
        assert_eq!(
            pane_click_bytes(Some(MouseProtocol::Sgr), 4, 7),
            Some(b"\x1b[<0;4;7M\x1b[<0;4;7m".to_vec())
        );
        assert_eq!(
            pane_click_bytes(Some(MouseProtocol::X10), 4, 7),
            Some(vec![
                b'\x1b', b'[', b'M', 32, 36, 39, b'\x1b', b'[', b'M', 35, 36, 39
            ])
        );
    }
}
