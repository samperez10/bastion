use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind, poll,
        read,
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
    collections::HashMap,
    io::{Read, Write},
    os::unix::net::UnixStream,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
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
    loop {
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
            DashboardExit::Attach { pane, roster } => {
                let mut active = pane;
                let mut roster = roster;
                notification_listener.set_active(Some(active.pane_id.clone()));
                loop {
                    match catch_unwind(AssertUnwindSafe(|| {
                        embedded_pane(terminal, state_dir, &active, &roster)
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
                        Ok(Ok(EmbeddedPaneExit::Focus {
                            pane_id,
                            roster: refreshed,
                        })) => {
                            roster = refreshed;
                            if let Some(next) =
                                roster.iter().find(|pane| pane.pane_id == pane_id).cloned()
                            {
                                active = next;
                                notification_listener.set_active(Some(active.pane_id.clone()));
                            } else {
                                notice = "pane is no longer available".to_owned();
                                break;
                            }
                        }
                    }
                }
                notification_listener.set_active(None);
            }
        }
    }
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
    let preferences = Preferences::load(state_dir)?;
    let mut sound_enabled = preferences.notifications.sound;
    let mut automatic_update_checks = preferences.updates.automatic_checks;
    loop {
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
            let masthead_height = workspace_masthead_height(frame.area().width);
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(masthead_height),
                    Constraint::Min(5),
                    Constraint::Length(2),
                ])
                .split(frame.area());
            render_workspace_masthead(frame, areas[0]);
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
                let dialog = workspace_removal_rect(frame.area());
                let root = workspaces[selected]
                    .get("canonical_root")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let name = std::path::Path::new(root)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(root);
                let panes = workspace_pane_count(&workspaces[selected]);
                frame.render_widget(WidgetClear, dialog);
                frame.render_widget(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(error()))
                        .title(" REMOVE WORKSPACE "),
                    dialog,
                );
                let inner = Rect::new(
                    dialog.x.saturating_add(2),
                    dialog.y.saturating_add(1),
                    dialog.width.saturating_sub(4),
                    dialog.height.saturating_sub(2),
                );
                frame.render_widget(
                    Paragraph::new(name)
                        .style(Style::default().fg(error()).add_modifier(Modifier::BOLD)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                frame.render_widget(
                    Paragraph::new(root)
                        .style(Style::default().fg(muted()))
                        .wrap(Wrap { trim: true }),
                    Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 2),
                );
                frame.render_widget(
                    Paragraph::new(if panes == 0 {
                        "Removes Bastion tabs and saved sessions."
                    } else {
                        "Stops active panes and removes Bastion state."
                    }),
                    Rect::new(inner.x, inner.y.saturating_add(4), inner.width, 1),
                );
                frame.render_widget(
                    Paragraph::new("Project files remain untouched.")
                        .style(Style::default().fg(muted())),
                    Rect::new(inner.x, inner.y.saturating_add(5), inner.width, 1),
                );
                let buttons = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(Rect::new(
                        dialog.x.saturating_add(1),
                        dialog.bottom().saturating_sub(2),
                        dialog.width.saturating_sub(2),
                        1,
                    ));
                frame.render_widget(
                    Paragraph::new("CANCEL")
                        .style(Style::default().fg(muted()))
                        .alignment(ratatui::layout::Alignment::Center),
                    buttons[0],
                );
                frame.render_widget(
                    Paragraph::new(if panes == 0 {
                        "REMOVE"
                    } else {
                        "STOP & REMOVE"
                    })
                    .style(Style::default().fg(error()).add_modifier(Modifier::BOLD))
                    .alignment(ratatui::layout::Alignment::Center),
                    buttons[1],
                );
            }
            if settings_open {
                let dialog = centered_fixed(54, 10, frame.area());
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
                    "{} Available update         {}",
                    if selected_setting == 3 { "›" } else { " " },
                    available_update
                        .as_deref()
                        .map(|version| format!("v{version}"))
                        .unwrap_or_else(|| "CURRENT".to_owned())
                );
                let update_checks = format!(
                    "{} Automatic update checks  {}",
                    if selected_setting == 2 { "›" } else { " " },
                    if automatic_update_checks { "ON" } else { "OFF" }
                );
                frame.render_widget(WidgetClear, dialog);
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled(
                            sound,
                            if selected_setting == 0 {
                                selected_style
                            } else {
                                normal_style
                            },
                        ),
                        Line::styled(
                            theme,
                            if selected_setting == 1 {
                                selected_style
                            } else {
                                normal_style
                            },
                        ),
                        Line::styled(
                            update_checks,
                            if selected_setting == 2 {
                                selected_style
                            } else {
                                normal_style
                            },
                        ),
                        Line::styled(
                            updates,
                            if selected_setting == 3 {
                                selected_style
                            } else {
                                normal_style
                            },
                        ),
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
                let dialog = centered_fixed(58, 11, frame.area());
                let version = available_update.as_deref().unwrap_or("new release");
                frame.render_widget(WidgetClear, dialog);
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            format!("Bastion v{version} is ready"),
                            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                        )),
                        Line::raw(""),
                        Line::raw("The download is checksum-verified before install."),
                        Line::raw("Running panes restart; saved agent sessions can resume."),
                        Line::raw(""),
                        Line::from(vec![
                            Span::styled("Esc ", Style::default().fg(muted())),
                            Span::raw("Later     "),
                            Span::styled("X ", Style::default().fg(muted())),
                            Span::raw("Skip     "),
                            Span::styled("Enter ", Style::default().fg(accent())),
                            Span::styled(
                                "Update now",
                                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                            ),
                        ]),
                    ])
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(accent()))
                            .title(" UPDATE AVAILABLE "),
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
                    selected_setting = (selected_setting + 1).min(3)
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
                    } else if selected_setting == 2 {
                        automatic_update_checks = !automatic_update_checks;
                        match set_update_check_preference(state_dir, automatic_update_checks) {
                            Ok(()) => {
                                hint = format!(
                                    " Automatic update checks: {} ",
                                    if automatic_update_checks { "ON" } else { "OFF" }
                                )
                            }
                            Err(error) => hint = format!(" Update setting failed: {error:#} "),
                        }
                    } else if available_update.is_some() {
                        settings_open = false;
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
                    let preferences = Preferences::load(state_dir)?;
                    sound_enabled = preferences.notifications.sound;
                    automatic_update_checks = preferences.updates.automatic_checks;
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
                let dialog = workspace_removal_rect(Rect::new(0, 0, size.width, size.height));
                let action_row = dialog.bottom().saturating_sub(2);
                if contains(dialog, mouse.column, mouse.row) && mouse.row == action_row {
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
                } else if !contains(dialog, mouse.column, mouse.row) {
                    removing = false;
                }
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && update_confirmation =>
            {
                let size = terminal.size()?;
                let dialog = centered_fixed(58, 11, Rect::new(0, 0, size.width, size.height));
                if !contains(dialog, mouse.column, mouse.row) {
                    update_confirmation = false;
                } else if mouse.row >= dialog.y.saturating_add(dialog.height.saturating_sub(5)) {
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
                let dialog = centered_fixed(54, 10, Rect::new(0, 0, size.width, size.height));
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
                    automatic_update_checks = !automatic_update_checks;
                    if let Err(error) =
                        set_update_check_preference(state_dir, automatic_update_checks)
                    {
                        hint = format!(" Update setting failed: {error:#} ");
                    }
                } else if mouse.row == dialog.y.saturating_add(4) {
                    selected_setting = 3;
                    if available_update.is_some() {
                        settings_open = false;
                        update_confirmation = true;
                    } else {
                        hint = " Bastion is up to date ".to_owned();
                    }
                } else if mouse.row >= dialog.y.saturating_add(6) {
                    settings_open = false;
                }
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && !removing
                    && mouse.row >= workspace_first_item_row(terminal.size()?.width) =>
            {
                let size = terminal.size()?;
                if mouse.row >= size.height.saturating_sub(2) {
                    let region = u32::from(mouse.column) * 3 / u32::from(size.width.max(1));
                    match region {
                        0 => {
                            settings_open = true;
                            selected_setting = 0;
                            let preferences = Preferences::load(state_dir)?;
                            sound_enabled = preferences.notifications.sound;
                            automatic_update_checks = preferences.updates.automatic_checks;
                        }
                        1 => removing = true,
                        _ => break Ok(None),
                    }
                    continue;
                }
                let first_item_row = workspace_first_item_row(size.width);
                let index = usize::from(mouse.row.saturating_sub(first_item_row));
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
    }
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

fn set_update_check_preference(state_dir: &std::path::Path, enabled: bool) -> Result<()> {
    let mut preferences = Preferences::load(state_dir)?;
    preferences.updates.automatic_checks = enabled;
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

fn listed_workspaces(state_dir: &Path) -> Result<Vec<serde_json::Value>> {
    Ok(dashboard_request(state_dir, Request::ListWorkspaces)?
        .get("workspaces")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn remove_workspace_request(state_dir: &Path, cwd: PathBuf, force: bool) -> Result<()> {
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

enum DashboardExit {
    Quit,
    Attach {
        pane: PaneDescriptor,
        roster: Vec<PaneDescriptor>,
    },
}

#[derive(Clone, Debug)]
struct PaneDescriptor {
    pane_id: String,
    label: String,
    agent: String,
    state: String,
    tab: String,
    workspace_root: String,
    resume_command: Option<String>,
}

enum EmbeddedPaneExit {
    Workspace,
    Reattach,
    Focus {
        pane_id: String,
        roster: Vec<PaneDescriptor>,
    },
}

#[derive(Clone, Copy, Default)]
struct PaneInputModes {
    application_cursor_keys: bool,
    bracketed_paste: bool,
}

enum PaneInputEvent {
    Reattach,
    Mouse(PaneMouseInput),
    Escape,
    Workspace,
    FocusPreview,
    ReturnToLiveBottom,
    Offline,
    Key(KeyEvent),
    Paste(String),
}

/// Keeps the low-latency input thread behind the UI thread while a touch is
/// being routed. Without this acknowledgement, a key pressed immediately
/// after opening a modal can be forwarded to the PTY before `chrome_active`
/// is set by the UI loop.
struct PaneMouseInput {
    event: MouseEvent,
    acknowledged: Option<mpsc::Sender<()>>,
}

impl std::ops::Deref for PaneMouseInput {
    type Target = MouseEvent;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl Drop for PaneMouseInput {
    fn drop(&mut self) {
        if let Some(acknowledged) = self.acknowledged.take() {
            let _ = acknowledged.send(());
        }
    }
}

enum PaneOverlay {
    Menu { selected: usize },
    Switch { selected: usize },
    Rename { value: String },
    Status,
    CloseConfirm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModalNavigation {
    Cancel,
    Previous,
    Next,
    Confirm,
    Ignore,
}

fn modal_navigation(code: &KeyCode) -> ModalNavigation {
    match code {
        KeyCode::Esc => ModalNavigation::Cancel,
        KeyCode::Up => ModalNavigation::Previous,
        KeyCode::Down => ModalNavigation::Next,
        KeyCode::Enter => ModalNavigation::Confirm,
        _ => ModalNavigation::Ignore,
    }
}

fn is_unmodified_text_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(_))
        && !key.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        )
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

struct PaneRosterPump {
    stop: mpsc::Sender<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for PaneRosterPump {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn start_pane_roster_pump(
    state_dir: PathBuf,
    workspace_root: String,
    tab: String,
) -> (PaneRosterPump, mpsc::Receiver<Vec<PaneDescriptor>>) {
    let (stop, stop_rx) = mpsc::channel();
    let (updates, update_rx) = mpsc::channel();
    let join = thread::spawn(move || {
        loop {
            if let Ok(status) = dashboard_request(&state_dir, Request::Status) {
                let mut values = status
                    .get("pane_details")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|pane| {
                        pane.get("workspace_root")
                            .and_then(serde_json::Value::as_str)
                            == Some(workspace_root.as_str())
                            && pane.get("tab").and_then(serde_json::Value::as_str)
                                == Some(tab.as_str())
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                values.sort_by_key(pane_order_key);
                if updates.send(pane_roster(&values)).is_err() {
                    break;
                }
            }
            match stop_rx.recv_timeout(Duration::from_millis(900)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    });
    (
        PaneRosterPump {
            stop,
            join: Some(join),
        },
        update_rx,
    )
}

fn start_pane_input_pump(
    writer: Arc<Mutex<UnixStream>>,
    modes: Arc<Mutex<PaneInputModes>>,
    chrome_active: Arc<AtomicBool>,
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
                Event::Mouse(mouse) => {
                    let (acknowledged_tx, acknowledged_rx) = mpsc::channel();
                    if sender
                        .send(PaneInputEvent::Mouse(PaneMouseInput {
                            event: mouse,
                            acknowledged: Some(acknowledged_tx),
                        }))
                        .is_err()
                    {
                        break;
                    }
                    while !stop_signal.load(Ordering::Acquire) {
                        match acknowledged_rx.recv_timeout(Duration::from_millis(20)) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                    }
                    None
                }
                Event::Paste(text) => {
                    if chrome_active.load(Ordering::Acquire) {
                        Some(PaneInputEvent::Paste(text))
                    } else {
                        let bracketed = modes.lock().unwrap().bracketed_paste;
                        let bytes = bracketed_paste_bytes(&text, bracketed);
                        if writer.lock().unwrap().write_all(&bytes).is_err() {
                            Some(PaneInputEvent::Offline)
                        } else {
                            Some(PaneInputEvent::ReturnToLiveBottom)
                        }
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if chrome_active.load(Ordering::Acquire) {
                        Some(PaneInputEvent::Key(key))
                    } else if key
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneLayout {
    header: Rect,
    terminal: Rect,
    rail: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneHeaderAreas {
    workspace: Rect,
    status: Rect,
    action: Rect,
}

fn pane_layout(area: Rect) -> PaneLayout {
    let header = Rect::new(area.x, area.y, area.width, area.height.min(2));
    let body = Rect::new(
        area.x,
        area.y.saturating_add(header.height),
        area.width,
        area.height.saturating_sub(header.height),
    );
    let show_rail = area.width >= 100 && area.width > area.height.saturating_mul(2);
    if show_rail {
        let rail_width = (area.width / 4).clamp(22, 30);
        PaneLayout {
            header,
            terminal: Rect::new(
                body.x,
                body.y,
                body.width.saturating_sub(rail_width),
                body.height,
            ),
            rail: Rect::new(
                body.right().saturating_sub(rail_width),
                body.y,
                rail_width,
                body.height,
            ),
        }
    } else {
        PaneLayout {
            header,
            terminal: body,
            rail: Rect::default(),
        }
    }
}

fn pane_header_areas(header: Rect) -> PaneHeaderAreas {
    let action_width = 5_u16.min(header.width / 4);
    let workspace = Rect::new(
        header.x,
        header.y,
        header.width.saturating_sub(action_width),
        header.height.min(1),
    );
    let action = Rect::new(
        workspace.right(),
        header.y,
        action_width,
        header.height.min(1),
    );
    let status = Rect::new(
        header.x,
        header.y.saturating_add(1),
        header.width,
        header.height.saturating_sub(1).min(1),
    );
    PaneHeaderAreas {
        workspace,
        status,
        action,
    }
}

fn pane_header_status(active: &PaneDescriptor, offline: bool, scrollback_offset: usize) -> String {
    if offline {
        format!("{} · OFFLINE", active.label)
    } else if scrollback_offset > 0 {
        format!("HISTORY · {scrollback_offset} lines up")
    } else if active.state == "unknown" || active.state.is_empty() {
        format!("{} · {}", active.label, active.agent)
    } else {
        format!("{} · {} · {}", active.label, active.agent, active.state)
    }
}

fn invalidate_pane_frame(
    retained_frame: &mut Option<Buffer>,
    prior_screen: &mut Option<workspace_terminal::Snapshot>,
) {
    *retained_frame = None;
    *prior_screen = None;
}

fn next_pane_id(roster: &[PaneDescriptor], active_id: &str) -> Option<String> {
    if roster.len() < 2 {
        return None;
    }
    let current = roster
        .iter()
        .position(|pane| pane.pane_id == active_id)
        .unwrap_or(0);
    roster
        .get((current + 1) % roster.len())
        .map(|pane| pane.pane_id.clone())
}

fn render_pane_rail(
    frame: &mut ratatui::Frame,
    area: Rect,
    active: &PaneDescriptor,
    roster: &[PaneDescriptor],
) {
    let project = std::path::Path::new(&active.workspace_root)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(border())),
        area,
    );
    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y,
        area.width.saturating_sub(3),
        area.height,
    );
    let mut lines = vec![
        Line::from(Span::styled(
            truncate_display_label(project, usize::from(inner.width)),
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate_display_label(&active.tab, usize::from(inner.width)),
            Style::default().fg(muted()),
        )),
        Line::raw(""),
        Line::from(Span::styled("PANES", Style::default().fg(muted()))),
    ];
    for pane in roster
        .iter()
        .take(usize::from(inner.height.saturating_sub(5)))
    {
        let selected = pane.pane_id == active.pane_id;
        let marker = if selected { "›" } else { " " };
        lines.push(Line::from(Span::styled(
            truncate_display_label(
                &format!("{marker} {} · {}", pane.label, pane.agent),
                usize::from(inner.width),
            ),
            if selected {
                Style::default()
                    .fg(accent())
                    .bg(focus_background())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            },
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn pane_overlay_rect(area: Rect, overlay: &PaneOverlay, roster_len: usize) -> Rect {
    match overlay {
        PaneOverlay::Menu { .. } => centered_fixed(44, 11, area),
        PaneOverlay::Switch { .. } => {
            centered_fixed(44, (roster_len as u16).saturating_add(4).min(14), area)
        }
        PaneOverlay::Rename { .. } | PaneOverlay::CloseConfirm => centered_fixed(44, 7, area),
        PaneOverlay::Status => centered_fixed(48, 11, area),
    }
}

fn render_pane_overlay(
    frame: &mut ratatui::Frame,
    area: Rect,
    overlay: &PaneOverlay,
    active: &PaneDescriptor,
    roster: &[PaneDescriptor],
) {
    let popup = pane_overlay_rect(area, overlay, roster.len());
    frame.render_widget(WidgetClear, popup);
    let lines = match overlay {
        PaneOverlay::Menu { selected } => {
            let labels = [
                "SWITCH PANE",
                "RENAME",
                "DETAILS",
                "CLOSE PANE",
                "WORKSPACE",
                "CANCEL",
            ];
            let mut lines = vec![
                Line::from(Span::styled(
                    active.label.clone(),
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "↑↓ Navigate · Enter · Esc",
                    Style::default().fg(muted()),
                )),
            ];
            lines.extend(labels.into_iter().enumerate().map(|(index, label)| {
                popup_menu_line(label, *selected == index, popup.width.saturating_sub(2))
            }));
            lines
        }
        PaneOverlay::Switch { selected } => {
            let mut lines = vec![Line::from(Span::styled(
                "Switch pane",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ))];
            lines.extend(roster.iter().enumerate().map(|(index, pane)| {
                popup_menu_line(
                    &format!("{} · {}", pane.label, pane.agent),
                    *selected == index,
                    popup.width.saturating_sub(2),
                )
            }));
            lines.push(Line::from(Span::styled(
                "Esc · back",
                Style::default().fg(muted()),
            )));
            lines
        }
        PaneOverlay::Rename { value } => vec![
            Line::from(Span::styled(
                "Rename pane",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(value.clone()),
            Line::raw(""),
            Line::from(Span::styled(
                "Enter · save     Esc · cancel",
                Style::default().fg(muted()),
            )),
        ],
        PaneOverlay::Status => vec![
            Line::from(Span::styled(
                active.label.clone(),
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            )),
            Line::raw(format!("Agent       {}", active.agent)),
            Line::raw(format!("State       {}", active.state)),
            Line::raw(format!("Tab         {}", active.tab)),
            Line::raw(format!("Pane ID     {}", active.pane_id)),
            Line::raw(""),
            Line::from(Span::styled(
                active
                    .resume_command
                    .as_deref()
                    .unwrap_or("No saved resume command"),
                Style::default().fg(muted()),
            )),
            Line::raw(""),
            Line::from(Span::styled("Esc · back", Style::default().fg(muted()))),
        ],
        PaneOverlay::CloseConfirm => vec![
            Line::from(Span::styled(
                format!("Close {}?", active.label),
                Style::default().fg(error()).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::raw("The running process will stop."),
            Line::raw(""),
            Line::from(vec![
                Span::styled("CANCEL", Style::default().fg(muted())),
                Span::raw("                 "),
                Span::styled("CLOSE", Style::default().fg(error())),
            ]),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent()))
                .title(" PANE "),
        ),
        popup,
    );
}

fn render_offline_pane(frame: &mut ratatui::Frame, area: Rect, active: &PaneDescriptor) {
    let popup = offline_pane_rect(area);
    frame.render_widget(WidgetClear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "PANE OFFLINE",
                Style::default().fg(error()).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::raw(format!("{} has stopped.", active.label)),
            Line::raw(""),
            Line::from(Span::styled(
                if active.resume_command.is_some() {
                    "WORKSPACE · saved session can resume"
                } else {
                    "WORKSPACE"
                },
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            )),
        ])
        .alignment(ratatui::layout::Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(error()))
                .title(" PANE "),
        ),
        popup,
    );
}

fn offline_pane_rect(area: Rect) -> Rect {
    centered_fixed(44, 7, area)
}

fn offline_pane_action_rect(area: Rect) -> Rect {
    let popup = offline_pane_rect(area);
    Rect::new(
        popup.x.saturating_add(1),
        popup.bottom().saturating_sub(2),
        popup.width.saturating_sub(2),
        1,
    )
}

fn embedded_pane(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state_dir: &Path,
    pane: &PaneDescriptor,
    roster: &[PaneDescriptor],
) -> Result<EmbeddedPaneExit> {
    let screen = terminal.size()?;
    let screen_area = Rect::new(0, 0, screen.width, screen.height);
    let layout = pane_layout(screen_area);
    let cols = layout.terminal.width.max(20);
    let rows = layout.terminal.height.max(4);
    let pane_id = pane.pane_id.clone();
    let mut active = pane.clone();
    let mut roster = roster.to_vec();
    let mut stream = UnixStream::connect(state_dir.join("workspace.sock"))?;
    serde_json::to_writer(
        &mut stream,
        &Request::Attach {
            pane_id: pane_id.clone(),
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
    let chrome_active = Arc::new(AtomicBool::new(false));
    let (_input_pump, input_rx) = start_pane_input_pump(
        Arc::clone(&writer),
        Arc::clone(&input_modes),
        Arc::clone(&chrome_active),
    );
    let (_roster_pump, roster_rx) = start_pane_roster_pump(
        state_dir.to_path_buf(),
        active.workspace_root.clone(),
        active.tab.clone(),
    );

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
    let mut overlay: Option<PaneOverlay> = None;
    loop {
        // Keep routing state derived from the authoritative UI state. The
        // touch acknowledgement below makes transitions into this state
        // atomic from the input thread's point of view.
        chrome_active.store(overlay.is_some(), Ordering::Release);
        while let Ok(updated) = roster_rx.try_recv() {
            if let Some(refreshed) = updated
                .iter()
                .find(|candidate| candidate.pane_id == pane_id)
                .cloned()
            {
                active = refreshed;
            }
            roster = updated;
            needs_draw = true;
        }
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
            let scrollback_offset = terminal_grid.display_offset();
            let dirty_rows = changed_terminal_rows(prior_screen.as_ref(), &active_screen);
            let full_terminal_draw = retained_frame.is_none();
            let completed = terminal.draw(|frame| {
                if let Some(previous) = retained_frame.as_ref() {
                    *frame.buffer_mut() = previous.clone();
                }
                let header = pane_header_areas(layout.header);
                // The retained-frame renderer only repaints changed terminal
                // rows. Clear Bastion's chrome explicitly so shorter status
                // labels never leave characters from the previous state.
                frame.render_widget(WidgetClear, layout.header);
                frame.render_widget(
                    Paragraph::new("‹ WORKSPACE")
                        .style(Style::default().fg(muted()))
                        .alignment(ratatui::layout::Alignment::Left),
                    header.workspace,
                );
                let center_text = pane_header_status(&active, offline, scrollback_offset);
                frame.render_widget(
                    Paragraph::new(truncate_display_label(
                        &center_text,
                        usize::from(header.status.width),
                    ))
                    .style(
                        Style::default()
                            .fg(if offline { error() } else { accent() })
                            .add_modifier(Modifier::BOLD),
                    )
                    .alignment(ratatui::layout::Alignment::Center),
                    header.status,
                );
                frame.render_widget(
                    Paragraph::new(if scrollback_offset > 0 { "LIVE" } else { "⋯" })
                        .style(Style::default().fg(accent()).add_modifier(Modifier::BOLD))
                        .alignment(ratatui::layout::Alignment::Center),
                    header.action,
                );
                render_terminal_grid(
                    frame,
                    layout.terminal,
                    &active_screen,
                    if full_terminal_draw {
                        None
                    } else {
                        Some(&dirty_rows)
                    },
                );
                if layout.rail.width > 0 {
                    render_pane_rail(frame, layout.rail, &active, &roster);
                }
                if let Some(overlay) = overlay.as_ref() {
                    render_pane_overlay(frame, screen_area, overlay, &active, &roster);
                } else if offline {
                    render_offline_pane(frame, screen_area, &active);
                }
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
            PaneInputEvent::Paste(text) => {
                if let Some(PaneOverlay::Rename { value }) = overlay.as_mut() {
                    value.push_str(&text);
                    invalidate_pane_frame(&mut retained_frame, &mut prior_screen);
                    needs_draw = true;
                }
            }
            PaneInputEvent::Key(key) => {
                // Modal chrome is painted over a retained terminal frame.
                // Invalidate that frame for every modal key so dismissing a
                // popup (or changing to a differently sized popup) cannot
                // leave stale pixels while input has already returned to the
                // PTY behind it.
                invalidate_pane_frame(&mut retained_frame, &mut prior_screen);
                let Some(current) = overlay.take() else {
                    chrome_active.store(false, Ordering::Release);
                    continue;
                };
                match current {
                    PaneOverlay::Menu { mut selected } => match modal_navigation(&key.code) {
                        ModalNavigation::Cancel => chrome_active.store(false, Ordering::Release),
                        ModalNavigation::Previous => {
                            selected = selected.saturating_sub(1);
                            overlay = Some(PaneOverlay::Menu { selected });
                        }
                        ModalNavigation::Next => {
                            selected = (selected + 1).min(5);
                            overlay = Some(PaneOverlay::Menu { selected });
                        }
                        ModalNavigation::Confirm => match selected {
                            0 => {
                                let selected = roster
                                    .iter()
                                    .position(|candidate| candidate.pane_id == pane_id)
                                    .unwrap_or(0);
                                overlay = Some(PaneOverlay::Switch { selected });
                            }
                            1 => {
                                overlay = Some(PaneOverlay::Rename {
                                    value: active.label.clone(),
                                })
                            }
                            2 => overlay = Some(PaneOverlay::Status),
                            3 => overlay = Some(PaneOverlay::CloseConfirm),
                            4 => return Ok(EmbeddedPaneExit::Workspace),
                            _ => chrome_active.store(false, Ordering::Release),
                        },
                        _ => overlay = Some(PaneOverlay::Menu { selected }),
                    },
                    PaneOverlay::Switch { mut selected } => match modal_navigation(&key.code) {
                        ModalNavigation::Cancel => {
                            overlay = Some(PaneOverlay::Menu { selected: 0 })
                        }
                        ModalNavigation::Previous => {
                            selected = selected.saturating_sub(1);
                            overlay = Some(PaneOverlay::Switch { selected });
                        }
                        ModalNavigation::Next => {
                            selected = (selected + 1).min(roster.len().saturating_sub(1));
                            overlay = Some(PaneOverlay::Switch { selected });
                        }
                        ModalNavigation::Confirm if !roster.is_empty() => {
                            return Ok(EmbeddedPaneExit::Focus {
                                pane_id: roster[selected].pane_id.clone(),
                                roster,
                            });
                        }
                        _ => overlay = Some(PaneOverlay::Switch { selected }),
                    },
                    PaneOverlay::Rename { mut value } => match key.code {
                        KeyCode::Esc => overlay = Some(PaneOverlay::Menu { selected: 1 }),
                        KeyCode::Backspace => {
                            value.pop();
                            overlay = Some(PaneOverlay::Rename { value });
                        }
                        KeyCode::Char(character) if is_unmodified_text_key(&key) => {
                            value.push(character);
                            overlay = Some(PaneOverlay::Rename { value });
                        }
                        KeyCode::Enter if !value.trim().is_empty() => {
                            let response = dashboard_request(
                                state_dir,
                                Request::RenamePane {
                                    pane_id: pane_id.clone(),
                                    name: value.trim().to_owned(),
                                },
                            )?;
                            if response.get("type").and_then(serde_json::Value::as_str)
                                == Some("error")
                            {
                                overlay = Some(PaneOverlay::Rename { value });
                            } else {
                                active.label = value.trim().to_owned();
                                chrome_active.store(false, Ordering::Release);
                            }
                        }
                        _ => overlay = Some(PaneOverlay::Rename { value }),
                    },
                    PaneOverlay::Status => match key.code {
                        KeyCode::Esc | KeyCode::Enter => {
                            overlay = Some(PaneOverlay::Menu { selected: 2 })
                        }
                        _ => overlay = Some(PaneOverlay::Status),
                    },
                    PaneOverlay::CloseConfirm => match key.code {
                        KeyCode::Esc => overlay = Some(PaneOverlay::Menu { selected: 3 }),
                        KeyCode::Enter => {
                            dashboard_request(
                                state_dir,
                                Request::StopPane {
                                    pane_id: pane_id.clone(),
                                },
                            )?;
                            return Ok(EmbeddedPaneExit::Workspace);
                        }
                        _ => overlay = Some(PaneOverlay::CloseConfirm),
                    },
                }
                needs_draw = true;
            }
            PaneInputEvent::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && overlay.is_some() =>
            {
                invalidate_pane_frame(&mut retained_frame, &mut prior_screen);
                let current = overlay.take().expect("overlay checked above");
                let popup = pane_overlay_rect(screen_area, &current, roster.len());
                if !contains(popup, mouse.column, mouse.row) {
                    chrome_active.store(false, Ordering::Release);
                    needs_draw = true;
                    continue;
                }
                match current {
                    PaneOverlay::Menu { selected } => {
                        let row = usize::from(mouse.row.saturating_sub(popup.y));
                        let choice = row.saturating_sub(3);
                        match choice {
                            0 => {
                                let selected = roster
                                    .iter()
                                    .position(|candidate| candidate.pane_id == pane_id)
                                    .unwrap_or(0);
                                overlay = Some(PaneOverlay::Switch { selected });
                            }
                            1 => {
                                overlay = Some(PaneOverlay::Rename {
                                    value: active.label.clone(),
                                })
                            }
                            2 => overlay = Some(PaneOverlay::Status),
                            3 => overlay = Some(PaneOverlay::CloseConfirm),
                            4 => return Ok(EmbeddedPaneExit::Workspace),
                            5 => chrome_active.store(false, Ordering::Release),
                            _ => overlay = Some(PaneOverlay::Menu { selected }),
                        }
                    }
                    PaneOverlay::Switch { selected } => {
                        let index =
                            usize::from(mouse.row.saturating_sub(popup.y.saturating_add(2)));
                        if let Some(next) = roster.get(index) {
                            return Ok(EmbeddedPaneExit::Focus {
                                pane_id: next.pane_id.clone(),
                                roster,
                            });
                        }
                        overlay = Some(PaneOverlay::Switch { selected });
                    }
                    PaneOverlay::Status => overlay = Some(PaneOverlay::Menu { selected: 2 }),
                    PaneOverlay::CloseConfirm => {
                        if mouse.row == popup.y.saturating_add(5)
                            && mouse.column >= popup.x.saturating_add(popup.width / 2)
                        {
                            dashboard_request(
                                state_dir,
                                Request::StopPane {
                                    pane_id: pane_id.clone(),
                                },
                            )?;
                            return Ok(EmbeddedPaneExit::Workspace);
                        }
                        overlay = Some(PaneOverlay::Menu { selected: 3 });
                    }
                    PaneOverlay::Rename { value } => overlay = Some(PaneOverlay::Rename { value }),
                }
                needs_draw = true;
            }
            PaneInputEvent::Reattach => return Ok(EmbeddedPaneExit::Reattach),
            PaneInputEvent::Mouse(mouse)
                if offline
                    && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && contains(
                        offline_pane_action_rect(screen_area),
                        mouse.column,
                        mouse.row,
                    ) =>
            {
                return Ok(EmbeddedPaneExit::Workspace);
            }
            PaneInputEvent::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) && contains(layout.terminal, mouse.column, mouse.row) =>
            {
                // A terminal viewport should scroll locally first. This is
                // the familiar Termux behavior and does not require every
                // agent TUI to implement touch/mouse scrolling itself.
                let x = mouse
                    .column
                    .saturating_sub(layout.terminal.x)
                    .min(cols.saturating_sub(1))
                    + 1;
                let y = mouse
                    .row
                    .saturating_sub(layout.terminal.y)
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
                    && contains(
                        pane_header_areas(layout.header).workspace,
                        mouse.column,
                        mouse.row,
                    ) =>
            {
                return Ok(EmbeddedPaneExit::Workspace);
            }
            PaneInputEvent::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && contains(
                        pane_header_areas(layout.header).action,
                        mouse.column,
                        mouse.row,
                    )
                    && terminal_grid.display_offset() > 0 =>
            {
                terminal_grid.scroll_viewport_to_bottom();
                needs_draw = true;
            }
            PaneInputEvent::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && contains(
                        pane_header_areas(layout.header).action,
                        mouse.column,
                        mouse.row,
                    ) =>
            {
                overlay = Some(PaneOverlay::Menu { selected: 0 });
                chrome_active.store(true, Ordering::Release);
                needs_draw = true;
            }
            PaneInputEvent::Mouse(mouse)
                if layout.rail.width > 0
                    && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && contains(layout.rail, mouse.column, mouse.row)
                    && mouse.row >= layout.rail.y.saturating_add(4) =>
            {
                let index = usize::from(mouse.row.saturating_sub(layout.rail.y + 4));
                if let Some(next) = roster.get(index)
                    && next.pane_id != pane_id
                {
                    return Ok(EmbeddedPaneExit::Focus {
                        pane_id: next.pane_id.clone(),
                        roster,
                    });
                }
            }
            PaneInputEvent::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && contains(layout.terminal, mouse.column, mouse.row) =>
            {
                // Claude/Codex fullscreen TUIs opt into terminal mouse input
                // for caret placement. Bastion owns its header, but content
                // taps belong to the PTY at its local 1-based cell position.
                let x = mouse
                    .column
                    .saturating_sub(layout.terminal.x)
                    .min(cols.saturating_sub(1))
                    + 1;
                let y = mouse
                    .row
                    .saturating_sub(layout.terminal.y)
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
                if let Some(next_id) = next_pane_id(&roster, &pane_id) {
                    return Ok(EmbeddedPaneExit::Focus {
                        pane_id: next_id,
                        roster,
                    });
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
    RenameTab,
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
    let mut selected_panes = HashMap::<String, String>::new();
    let mut pane_offsets = HashMap::<String, usize>::new();
    let mut input: Option<DashboardInput> = None;
    let mut manage_open = false;
    let mut pane_menu_selected = 0_usize;
    let mut tab_menu_open = false;
    let mut tab_menu_selected = 0_usize;
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
        let mut panes: Vec<serde_json::Value> = status
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
        panes.sort_by_key(pane_order_key);
        let resumable_slots = saved_session_slots(&slots);
        let mut selected_pane = selected_panes
            .get(tab_name)
            .and_then(|selected_id| {
                panes
                    .iter()
                    .position(|pane| pane_id(pane) == Some(selected_id))
            })
            .unwrap_or(0)
            .min(panes.len().saturating_sub(1));
        if let Some(id) = panes.get(selected_pane).and_then(pane_id) {
            selected_panes.insert(tab_name.to_owned(), id.to_owned());
        }
        selected_resume = selected_resume.min(resumable_slots.len().saturating_sub(1));
        let screen_size = terminal.size()?;
        let screen = dashboard_rect(Rect::new(0, 0, screen_size.width, screen_size.height));
        let areas = window_areas(screen);
        let tab_hit_areas = dashboard_tab_areas(areas.tabs, tab_values.len(), selected_tab);
        let action_hit_areas = dashboard_action_areas(areas.actions);
        let visible_panes = usize::from(areas.panes.height.saturating_sub(2) / 2).max(1);
        let pane_offset = pane_offsets.entry(tab_name.to_owned()).or_default();
        if selected_pane < *pane_offset {
            *pane_offset = selected_pane;
        } else if selected_pane >= pane_offset.saturating_add(visible_panes) {
            *pane_offset = selected_pane
                .saturating_add(1)
                .saturating_sub(visible_panes);
        }
        *pane_offset = (*pane_offset).min(panes.len().saturating_sub(visible_panes));
        terminal.draw(|frame| {
            draw_window_dashboard(
                frame,
                screen,
                DashboardView {
                    tabs: &tab_values,
                    selected_tab,
                    panes: &panes,
                    selected_pane,
                    pane_offset: *pane_offset,
                    notice: notice.as_str(),
                    input: input.as_ref(),
                    manage_open,
                    pane_menu_selected,
                    tab_menu_open,
                    tab_menu_selected,
                    resume_picker_open,
                    resumable_slots: &resumable_slots,
                    selected_resume,
                    workspace,
                },
            );
        })?;

        if !poll(Duration::from_millis(750))? {
            continue;
        }
        match read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(active_input) = input.as_mut() {
                    let confirmation = matches!(
                        active_input.action,
                        DashboardAction::DeleteTab | DashboardAction::StopPane
                    );
                    match key.code {
                        KeyCode::Esc => input = None,
                        KeyCode::Backspace if !confirmation => {
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
                        KeyCode::Char(character)
                            if !confirmation && is_unmodified_text_key(&key) =>
                        {
                            active_input.value.push(character)
                        }
                        _ => {}
                    }
                    continue;
                }
                if resume_picker_open {
                    match modal_navigation(&key.code) {
                        ModalNavigation::Cancel => resume_picker_open = false,
                        ModalNavigation::Previous => {
                            selected_resume = selected_resume.saturating_sub(1)
                        }
                        ModalNavigation::Next => {
                            selected_resume =
                                (selected_resume + 1).min(resumable_slots.len().saturating_sub(1));
                        }
                        ModalNavigation::Confirm if !resumable_slots.is_empty() => {
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
                if tab_menu_open {
                    match modal_navigation(&key.code) {
                        ModalNavigation::Cancel => tab_menu_open = false,
                        ModalNavigation::Previous => {
                            tab_menu_selected = tab_menu_selected.saturating_sub(1)
                        }
                        ModalNavigation::Next => tab_menu_selected = (tab_menu_selected + 1).min(2),
                        ModalNavigation::Confirm => {
                            match tab_menu_selected {
                                0 => input = Some(rename_tab_input(tab_name)),
                                1 => input = Some(new_dashboard_input(DashboardAction::DeleteTab)),
                                _ => {}
                            }
                            tab_menu_open = false;
                        }
                        _ => {}
                    }
                    continue;
                }
                if manage_open {
                    match modal_navigation(&key.code) {
                        ModalNavigation::Cancel => manage_open = false,
                        ModalNavigation::Previous => {
                            pane_menu_selected = pane_menu_selected.saturating_sub(1)
                        }
                        ModalNavigation::Next => {
                            pane_menu_selected = (pane_menu_selected + 1).min(3)
                        }
                        ModalNavigation::Confirm if !panes.is_empty() => {
                            match pane_menu_selected {
                                0 => {
                                    input = selected_pane_input(
                                        DashboardAction::RenamePane,
                                        &panes[selected_pane],
                                    )
                                }
                                1 => *notice = pane_status(&panes[selected_pane]),
                                2 => {
                                    input = selected_pane_input(
                                        DashboardAction::StopPane,
                                        &panes[selected_pane],
                                    )
                                }
                                _ => {}
                            }
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
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        selected_tab = (selected_tab + 1).min(tab_values.len() - 1);
                        *remembered_tab = selected_tab;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected_pane = selected_pane.saturating_sub(1);
                        remember_selected_pane(
                            &mut selected_panes,
                            tab_name,
                            &panes,
                            selected_pane,
                        );
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected_pane = (selected_pane + 1).min(panes.len().saturating_sub(1));
                        remember_selected_pane(
                            &mut selected_panes,
                            tab_name,
                            &panes,
                            selected_pane,
                        );
                    }
                    KeyCode::Enter if !panes.is_empty() => {
                        let pane =
                            pane_descriptor(&panes[selected_pane]).context("pane has no ID")?;
                        return Ok(DashboardExit::Attach {
                            pane,
                            roster: pane_roster(&panes),
                        });
                    }
                    KeyCode::Char('n') => {
                        input = Some(new_dashboard_input(DashboardAction::NewTab))
                    }
                    KeyCode::Char('x') => {
                        tab_menu_selected = 0;
                        tab_menu_open = true;
                    }
                    KeyCode::Char('m') if !panes.is_empty() => {
                        pane_menu_selected = 0;
                        manage_open = true;
                    }
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
                    let menu = resume_menu_rect(screen, resumable_slots.len());
                    let row = usize::from(mouse.row.saturating_sub(menu.y));
                    let (slot_offset, visible_slots) =
                        resume_menu_viewport(selected_resume, resumable_slots.len(), menu);
                    let visible_index = row.saturating_sub(4);
                    let slot_index = slot_offset.saturating_add(visible_index);
                    if row >= 4
                        && visible_index < visible_slots
                        && slot_index < resumable_slots.len()
                    {
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
                if tab_menu_open {
                    let menu = tab_menu_rect(screen);
                    if contains(menu, mouse.column, mouse.row) {
                        match mouse.row.saturating_sub(menu.y) {
                            4 => input = Some(rename_tab_input(tab_name)),
                            5 => input = Some(new_dashboard_input(DashboardAction::DeleteTab)),
                            _ => {}
                        }
                    }
                    tab_menu_open = false;
                    continue;
                }
                if manage_open {
                    let menu = pane_menu_rect(screen);
                    if contains(menu, mouse.column, mouse.row) {
                        match mouse.row.saturating_sub(menu.y) {
                            4 if !panes.is_empty() => {
                                input = selected_pane_input(
                                    DashboardAction::RenamePane,
                                    &panes[selected_pane],
                                );
                            }
                            5 if !panes.is_empty() => {
                                *notice = pane_status(&panes[selected_pane]);
                            }
                            6 if !panes.is_empty() => {
                                input = selected_pane_input(
                                    DashboardAction::StopPane,
                                    &panes[selected_pane],
                                );
                            }
                            _ => {}
                        }
                    }
                    manage_open = false;
                    continue;
                }
                if contains(areas.tabs, mouse.column, mouse.row) {
                    if contains(tab_hit_areas.add, mouse.column, mouse.row) {
                        input = Some(new_dashboard_input(DashboardAction::NewTab));
                    } else if contains(tab_hit_areas.manage, mouse.column, mouse.row) {
                        tab_menu_selected = 0;
                        tab_menu_open = true;
                    } else if tab_hit_areas.compact
                        && contains(tab_hit_areas.previous, mouse.column, mouse.row)
                    {
                        selected_tab = selected_tab.saturating_sub(1);
                    } else if tab_hit_areas.compact
                        && contains(tab_hit_areas.next, mouse.column, mouse.row)
                    {
                        selected_tab = (selected_tab + 1).min(tab_values.len() - 1);
                    } else if let Some((index, _)) = tab_hit_areas
                        .tabs
                        .iter()
                        .find(|(_, rect)| contains(*rect, mouse.column, mouse.row))
                    {
                        selected_tab = *index;
                    }
                    *remembered_tab = selected_tab;
                } else if contains(areas.panes, mouse.column, mouse.row) && !panes.is_empty() {
                    let relative_row = mouse.row.saturating_sub(areas.panes.y.saturating_add(1));
                    let index = pane_offset.saturating_add(usize::from(relative_row / 2));
                    if index < panes.len() {
                        selected_pane = index;
                        remember_selected_pane(
                            &mut selected_panes,
                            tab_name,
                            &panes,
                            selected_pane,
                        );
                        let menu_column = areas
                            .panes
                            .x
                            .saturating_add(areas.panes.width.saturating_sub(7));
                        if mouse.column >= menu_column {
                            pane_menu_selected = 0;
                            manage_open = true;
                            continue;
                        }
                        let pane = pane_descriptor(&panes[index]).context("pane has no ID")?;
                        return Ok(DashboardExit::Attach {
                            pane,
                            roster: pane_roster(&panes),
                        });
                    }
                } else if contains(areas.actions, mouse.column, mouse.row) {
                    if mouse.row != areas.actions.y.saturating_add(1) {
                        continue;
                    }
                    if contains(action_hit_areas[0], mouse.column, mouse.row) {
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
                    } else if contains(action_hit_areas[1], mouse.column, mouse.row) {
                        if resumable_slots.is_empty() {
                            *notice = "no saved agent sessions in this workspace".to_owned()
                        } else {
                            resume_picker_open = true;
                        }
                    } else if contains(action_hit_areas[2], mouse.column, mouse.row) {
                        return Ok(DashboardExit::Quit);
                    }
                }
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) && contains(areas.panes, mouse.column, mouse.row)
                    && !panes.is_empty() =>
            {
                selected_pane = scrolled_pane_selection(
                    selected_pane,
                    panes.len(),
                    matches!(mouse.kind, MouseEventKind::ScrollUp),
                );
                remember_selected_pane(&mut selected_panes, tab_name, &panes, selected_pane);
            }
            _ => {}
        }
    }
}

struct WindowAreas {
    identity: Rect,
    tabs: Rect,
    panes: Rect,
    details: Rect,
    actions: Rect,
    notice: Rect,
}

fn dashboard_rect(screen: Rect) -> Rect {
    screen
}

fn wide_dashboard(area: Rect) -> bool {
    area.width >= 72 && area.width > area.height.saturating_mul(2)
}

fn window_areas(area: Rect) -> WindowAreas {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    let body = if wide_dashboard(area) {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(chunks[2])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100), Constraint::Length(0)])
            .split(chunks[2])
    };
    WindowAreas {
        identity: chunks[0],
        tabs: chunks[1],
        panes: body[0],
        details: body[1],
        actions: chunks[3],
        notice: chunks[4],
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

fn rename_tab_input(name: &str) -> DashboardInput {
    DashboardInput {
        action: DashboardAction::RenameTab,
        value: name.to_owned(),
        target: Some(name.to_owned()),
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
    centered_fixed(40, 9, area)
}

fn tab_menu_rect(area: Rect) -> Rect {
    centered_fixed(38, 8, area)
}

fn popup_menu_line(label: &str, selected: bool, width: u16) -> Line<'static> {
    let text = format!(
        "  {label:<width$}",
        width = usize::from(width.saturating_sub(2))
    );
    Line::from(Span::styled(
        text,
        if selected {
            Style::default()
                .fg(accent())
                .bg(focus_background())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        },
    ))
}

fn resume_menu_rect(area: Rect, slot_count: usize) -> Rect {
    let visible_slots = slot_count.clamp(1, 5) as u16;
    centered_fixed(44, visible_slots.saturating_add(7), area)
}

fn resume_menu_viewport(selected: usize, slot_count: usize, menu: Rect) -> (usize, usize) {
    let visible = usize::from(menu.height.saturating_sub(7)).min(slot_count);
    if visible == 0 {
        return (0, 0);
    }
    let offset = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(slot_count.saturating_sub(visible));
    (offset, visible)
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
    state_dir: &Path,
    workspace: &Path,
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
        DashboardAction::RenameTab => "Rename tab",
        DashboardAction::NewShell => "New shell",
        DashboardAction::Resume => "Resume: agent slot",
        DashboardAction::DeleteTab => "Delete selected tab? Enter confirms · Esc cancels",
        DashboardAction::StopPane => "Stop selected pane? Enter confirms · Esc cancels",
        DashboardAction::RenamePane => "Pane name",
    }
}

fn execute_dashboard_action(
    action: DashboardAction,
    value: &str,
    target: Option<&str>,
    state_dir: &Path,
    workspace: &Path,
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
        DashboardAction::RenameTab => {
            let original = target.context("no tab is selected")?;
            dashboard_request(
                state_dir,
                Request::RenameTab {
                    name: original.to_owned(),
                    new_name: value.trim().to_owned(),
                    cwd: Some(workspace.to_path_buf()),
                },
            )?
        }
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

struct DashboardTabAreas {
    compact: bool,
    previous: Rect,
    next: Rect,
    tabs: Vec<(usize, Rect)>,
    add: Rect,
    manage: Rect,
}

fn dashboard_action_areas(area: Rect) -> [Rect; 3] {
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    );
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(inner);
    [chunks[0], chunks[1], chunks[2]]
}

fn dashboard_tab_areas(area: Rect, tab_count: usize, selected_tab: usize) -> DashboardTabAreas {
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    );
    let control_width = 5_u16.min(inner.width / 4);
    let manage = Rect::new(
        inner
            .x
            .saturating_add(inner.width.saturating_sub(control_width)),
        inner.y,
        control_width,
        1,
    );
    let add = Rect::new(
        manage.x.saturating_sub(control_width),
        inner.y,
        control_width,
        1,
    );
    let tab_zone = Rect::new(
        inner.x,
        inner.y,
        inner.width.saturating_sub(control_width.saturating_mul(2)),
        1,
    );
    let compact =
        area.width < 64 || tab_count == 0 || usize::from(tab_zone.width) / tab_count.max(1) < 8;
    if compact {
        let arrow_width = 4_u16.min(tab_zone.width / 3);
        let previous = Rect::new(tab_zone.x, tab_zone.y, arrow_width, 1);
        let next = Rect::new(
            tab_zone
                .x
                .saturating_add(tab_zone.width.saturating_sub(arrow_width)),
            tab_zone.y,
            arrow_width,
            1,
        );
        let current = Rect::new(
            previous.x.saturating_add(previous.width),
            tab_zone.y,
            tab_zone.width.saturating_sub(arrow_width.saturating_mul(2)),
            1,
        );
        return DashboardTabAreas {
            compact,
            previous,
            next,
            tabs: vec![(selected_tab, current)],
            add,
            manage,
        };
    }

    let mut tabs = Vec::with_capacity(tab_count);
    let mut x = tab_zone.x;
    for index in 0..tab_count {
        let remaining = tab_zone.x.saturating_add(tab_zone.width).saturating_sub(x);
        let remaining_tabs = (tab_count - index) as u16;
        let width = remaining / remaining_tabs.max(1);
        tabs.push((index, Rect::new(x, tab_zone.y, width, 1)));
        x = x.saturating_add(width);
    }
    DashboardTabAreas {
        compact,
        previous: Rect::default(),
        next: Rect::default(),
        tabs,
        add,
        manage,
    }
}

fn truncate_display_label(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }
    let content_width = max_width - 1;
    let mut used = 0_usize;
    let mut output = String::new();
    for character in value.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(width) > content_width {
            break;
        }
        output.push(character);
        used = used.saturating_add(width);
    }
    output.push('…');
    output
}

fn pane_id(pane: &serde_json::Value) -> Option<&str> {
    pane.get("pane_id").and_then(serde_json::Value::as_str)
}

fn pane_label(pane: &serde_json::Value) -> &str {
    pane.get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Pane")
}

fn pane_descriptor(pane: &serde_json::Value) -> Option<PaneDescriptor> {
    Some(PaneDescriptor {
        pane_id: pane_id(pane)?.to_owned(),
        label: pane_label(pane).to_owned(),
        agent: pane
            .get("agent_kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("shell")
            .to_owned(),
        state: pane
            .get("agent_state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        tab: pane
            .get("tab")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Main")
            .to_owned(),
        workspace_root: pane
            .get("workspace_root")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        resume_command: pane
            .get("resume_command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

fn pane_roster(panes: &[serde_json::Value]) -> Vec<PaneDescriptor> {
    panes.iter().filter_map(pane_descriptor).collect()
}

fn pane_order_key(pane: &serde_json::Value) -> (u8, u64, String, String) {
    let label = pane_label(pane);
    let numbered = label
        .strip_prefix("Pane ")
        .and_then(|number| number.parse::<u64>().ok());
    (
        u8::from(numbered.is_none()),
        numbered.unwrap_or(u64::MAX),
        label.to_ascii_lowercase(),
        pane_id(pane).unwrap_or_default().to_owned(),
    )
}

fn remember_selected_pane(
    selected: &mut HashMap<String, String>,
    tab: &str,
    panes: &[serde_json::Value],
    index: usize,
) {
    if let Some(id) = panes.get(index).and_then(pane_id) {
        selected.insert(tab.to_owned(), id.to_owned());
    }
}

fn scrolled_pane_selection(selected: usize, pane_count: usize, upward: bool) -> usize {
    if upward {
        selected.saturating_sub(1)
    } else {
        (selected + 1).min(pane_count.saturating_sub(1))
    }
}

fn pane_semantic_state(pane: &serde_json::Value) -> (&'static str, String, Color) {
    let agent = pane.get("agent_kind").and_then(serde_json::Value::as_str);
    let state = pane
        .get("agent_state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let health = pane
        .get("health")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("waiting");
    let (marker, label, color) = match (agent, state, health) {
        (None, _, "active") => ("◇", "shell".to_owned(), success()),
        (None, _, _) => ("◇", "shell".to_owned(), muted()),
        (_, "attention", _) => ("◆", "attention".to_owned(), warning()),
        (_, "working", _) => ("●", "working".to_owned(), success()),
        (_, "done", _) => ("✓", "done".to_owned(), accent()),
        (_, "idle", _) => ("○", "idle".to_owned(), muted()),
        (_, _, "active") => ("●", "active".to_owned(), success()),
        _ => ("◌", "waiting".to_owned(), muted()),
    };
    (marker, label, color)
}

struct DashboardView<'a> {
    tabs: &'a [serde_json::Value],
    selected_tab: usize,
    panes: &'a [serde_json::Value],
    selected_pane: usize,
    pane_offset: usize,
    notice: &'a str,
    input: Option<&'a DashboardInput>,
    manage_open: bool,
    pane_menu_selected: usize,
    tab_menu_open: bool,
    tab_menu_selected: usize,
    resume_picker_open: bool,
    resumable_slots: &'a [serde_json::Value],
    selected_resume: usize,
    workspace: &'a Path,
}

fn draw_window_dashboard(frame: &mut ratatui::Frame, area: Rect, view: DashboardView<'_>) {
    let DashboardView {
        tabs,
        selected_tab,
        panes,
        selected_pane,
        pane_offset,
        notice,
        input,
        manage_open,
        pane_menu_selected,
        tab_menu_open,
        tab_menu_selected,
        resume_picker_open,
        resumable_slots,
        selected_resume,
        workspace,
    } = view;
    let areas = window_areas(area);
    let project = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");

    let identity_inner = Rect::new(
        areas.identity.x.saturating_add(1),
        areas.identity.y.saturating_add(1),
        areas.identity.width.saturating_sub(2),
        areas.identity.height.saturating_sub(2),
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border())),
        areas.identity,
    );
    let identity_left = format!(" ▪ ▪ ▪  {}", project.to_ascii_uppercase());
    let identity_right = "WORKSPACE ";
    let identity_spacing = " ".repeat(
        usize::from(identity_inner.width)
            .saturating_sub(identity_left.chars().count())
            .saturating_sub(identity_right.chars().count()),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                identity_left,
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ),
            Span::raw(identity_spacing),
            Span::styled(identity_right, Style::default().fg(muted())),
        ])),
        Rect::new(identity_inner.x, identity_inner.y, identity_inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(format!(" {}", workspace.display())).style(Style::default().fg(muted())),
        Rect::new(
            identity_inner.x,
            identity_inner.y.saturating_add(1),
            identity_inner.width,
            1,
        ),
    );

    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border()))
            .title(" TABS "),
        areas.tabs,
    );
    let tab_areas = dashboard_tab_areas(areas.tabs, tabs.len(), selected_tab);
    if tab_areas.compact {
        frame.render_widget(
            Paragraph::new("‹")
                .style(Style::default().fg(muted()))
                .alignment(ratatui::layout::Alignment::Center),
            tab_areas.previous,
        );
        frame.render_widget(
            Paragraph::new("›")
                .style(Style::default().fg(muted()))
                .alignment(ratatui::layout::Alignment::Center),
            tab_areas.next,
        );
    }
    for (index, tab_area) in &tab_areas.tabs {
        let name = tabs
            .get(*index)
            .and_then(|tab| tab.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Main");
        let text = if tab_areas.compact {
            let position = format!("  {}/{}", selected_tab + 1, tabs.len());
            let label_width = usize::from(tab_area.width)
                .saturating_sub(UnicodeWidthStr::width(position.as_str()));
            format!("{}{}", truncate_display_label(name, label_width), position)
        } else {
            truncate_display_label(name, usize::from(tab_area.width))
        };
        let selected = *index == selected_tab;
        frame.render_widget(
            Paragraph::new(text)
                .style(if selected {
                    Style::default()
                        .fg(accent())
                        .bg(focus_background())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(muted())
                })
                .alignment(ratatui::layout::Alignment::Center),
            *tab_area,
        );
    }
    frame.render_widget(
        Paragraph::new("+")
            .style(Style::default().fg(accent()).add_modifier(Modifier::BOLD))
            .alignment(ratatui::layout::Alignment::Center),
        tab_areas.add,
    );
    frame.render_widget(
        Paragraph::new("⋯")
            .style(Style::default().fg(muted()))
            .alignment(ratatui::layout::Alignment::Center),
        tab_areas.manage,
    );

    let pane_inner_width = areas.panes.width.saturating_sub(2);
    let pane_items = if panes.is_empty() {
        vec![ListItem::new(vec![
            Line::raw(""),
            Line::from(Span::styled(
                "  No panes in this tab",
                Style::default().fg(muted()),
            )),
            Line::from(Span::styled(
                "  Start a shell with + PANE or restore a saved session",
                Style::default().fg(muted()),
            )),
        ])]
    } else {
        panes
            .iter()
            .map(|pane| {
                let label = pane_label(pane);
                let agent = pane
                    .get("agent_kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("shell");
                let (marker, state, state_color) = pane_semantic_state(pane);
                let reserved = 8_usize;
                let used = marker.chars().count() + label.chars().count() + 3;
                let spacing = " ".repeat(
                    usize::from(pane_inner_width)
                        .saturating_sub(used)
                        .saturating_sub(reserved),
                );
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(format!(" {marker} "), Style::default().fg(state_color)),
                        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(spacing),
                        Span::styled("  ⋯  ", Style::default().fg(muted())),
                    ]),
                    Line::from(vec![
                        Span::raw("   "),
                        Span::styled(
                            format!("{agent} · {state}"),
                            Style::default().fg(state_color),
                        ),
                    ]),
                ])
            })
            .collect()
    };
    let pane_list = List::new(pane_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border()))
                .title(" PANES · tap to open "),
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
        *pane_state.offset_mut() = pane_offset;
    }
    frame.render_stateful_widget(pane_list, areas.panes, &mut pane_state);

    if areas.details.width > 0 {
        let detail_lines = panes.get(selected_pane).map_or_else(
            || {
                vec![
                    Line::raw(""),
                    Line::from(Span::styled(
                        "No pane selected",
                        Style::default().fg(muted()),
                    )),
                ]
            },
            |pane| {
                let agent = pane
                    .get("agent_kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("shell");
                let (_, state, state_color) = pane_semantic_state(pane);
                let command = pane
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("shell");
                let idle = pane
                    .get("idle_seconds")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                vec![
                    Line::from(Span::styled(
                        pane_label(pane).to_owned(),
                        Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        format!("{agent} · {state}"),
                        Style::default().fg(state_color),
                    )),
                    Line::raw(""),
                    Line::from(Span::styled("COMMAND", Style::default().fg(muted()))),
                    Line::from(command.to_owned()),
                    Line::raw(""),
                    Line::from(Span::styled(
                        format!("Last activity {idle}s ago"),
                        Style::default().fg(muted()),
                    )),
                    Line::from(Span::styled(
                        "Enter opens · M manages",
                        Style::default().fg(muted()),
                    )),
                ]
            },
        );
        frame.render_widget(
            Paragraph::new(detail_lines)
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border()))
                        .title(" PANE DETAILS "),
                ),
            areas.details,
        );
    }

    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border())),
        areas.actions,
    );
    for (index, (label, action_area)) in ["+ PANE", "RESUME", "WORKSPACES"]
        .into_iter()
        .zip(dashboard_action_areas(areas.actions))
        .enumerate()
    {
        let enabled = index != 1 || !resumable_slots.is_empty();
        frame.render_widget(
            Paragraph::new(label)
                .style(if index == 0 {
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD)
                } else if enabled {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(muted())
                })
                .alignment(ratatui::layout::Alignment::Center),
            action_area,
        );
    }
    frame.render_widget(
        Paragraph::new(notice)
            .style(Style::default().fg(if notice.is_empty() {
                muted()
            } else {
                warning()
            }))
            .wrap(Wrap { trim: true }),
        areas.notice,
    );
    if let Some(input) = input {
        let popup = if matches!(
            input.action,
            DashboardAction::DeleteTab | DashboardAction::StopPane
        ) {
            centered_fixed(48, 7, area)
        } else {
            centered_fixed(44, 7, area)
        };
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
        let menu = resume_menu_rect(area, resumable_slots.len());
        let (slot_offset, visible_slots) =
            resume_menu_viewport(selected_resume, resumable_slots.len(), menu);
        frame.render_widget(WidgetClear, menu);
        let entries = resumable_slots
            .iter()
            .enumerate()
            .skip(slot_offset)
            .take(visible_slots)
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
                if visible_slots < resumable_slots.len() {
                    format!(
                        "Sessions {}–{} of {}",
                        slot_offset + 1,
                        slot_offset + visible_slots,
                        resumable_slots.len()
                    )
                } else {
                    "Tap a session to resume it".to_owned()
                },
                Style::default().fg(muted()),
            )),
            Line::raw(""),
        ];
        lines.extend(entries);
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "↑↓ Navigate · Enter select · Esc back",
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
    } else if tab_menu_open {
        let label = tabs
            .get(selected_tab)
            .and_then(|tab| tab.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Tab");
        let menu = tab_menu_rect(area);
        frame.render_widget(WidgetClear, menu);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    label.to_owned(),
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "↑↓ Navigate · Enter · Esc",
                    Style::default().fg(muted()),
                )),
                Line::raw(""),
                popup_menu_line("RENAME TAB", tab_menu_selected == 0, menu.width - 2),
                popup_menu_line("DELETE TAB", tab_menu_selected == 1, menu.width - 2),
                popup_menu_line("CANCEL", tab_menu_selected == 2, menu.width - 2),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(accent()))
                    .title(" Tab menu "),
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
                    "↑↓ Navigate · Enter · Esc",
                    Style::default().fg(muted()),
                )),
                Line::raw(""),
                popup_menu_line("RENAME", pane_menu_selected == 0, menu.width - 2),
                popup_menu_line("STATUS", pane_menu_selected == 1, menu.width - 2),
                popup_menu_line("CLOSE PANE", pane_menu_selected == 2, menu.width - 2),
                popup_menu_line("CANCEL", pane_menu_selected == 3, menu.width - 2),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(accent()))
                    .title(" Pane menu "),
            ),
            menu,
        );
    }
}

const MASTHEAD_DESCRIPTION: &str = "Persistent workspaces for AI agents";
const MASTHEAD_PROMISE: &str = "ONE COMMAND CENTER FOR EVERY AGENT";

fn workspace_masthead_height(width: u16) -> u16 {
    if width >= 48 {
        9
    } else if width >= 40 {
        13
    } else {
        15
    }
}

fn workspace_first_item_row(width: u16) -> u16 {
    workspace_masthead_height(width).saturating_add(1)
}

/// Render Bastion's fixed identity at full terminal width. Wide screens keep
/// the gate mark beside the copy; portrait widths reflow the complete mark
/// above complete, unabridged text rather than cropping either one.
fn render_workspace_masthead(frame: &mut ratatui::Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border()));
    frame.render_widget(block, area);
    if area.width < 3 || area.height < 4 {
        return;
    }

    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let left = "  ▪ ▪ ▪";
    let right = "B A S T I O N  ";
    let spacing = " ".repeat(
        usize::from(inner.width)
            .saturating_sub(left.chars().count())
            .saturating_sub(right.chars().count()),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                left,
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ),
            Span::raw(spacing),
            Span::styled(
                right,
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let separator = format!(
        "├{}┤",
        "─".repeat(usize::from(area.width.saturating_sub(2)))
    );
    frame.render_widget(
        Paragraph::new(Line::styled(separator, Style::default().fg(border()))),
        Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
    );

    if area.width >= 48 {
        let gap = 2_u16;
        let logo_width = inner
            .width
            .saturating_sub(gap)
            .saturating_sub(MASTHEAD_DESCRIPTION.chars().count() as u16)
            .clamp(6, 18);
        let copy_x = inner.x.saturating_add(logo_width).saturating_add(gap);
        let copy_width = inner.width.saturating_sub(logo_width).saturating_sub(gap);
        for (offset, (mark, copy)) in [
            ("▟█▙▟█▙", ">_ CODE · BUILD · DEPLOY"),
            ("██████", MASTHEAD_DESCRIPTION),
            ("██▛▜██", MASTHEAD_PROMISE),
        ]
        .into_iter()
        .enumerate()
        {
            frame.render_widget(
                Paragraph::new(Line::styled(mark, Style::default().fg(accent())))
                    .alignment(ratatui::layout::Alignment::Center),
                Rect::new(
                    inner.x,
                    area.y.saturating_add(4 + offset as u16),
                    logo_width,
                    1,
                ),
            );
            frame.render_widget(
                Paragraph::new(Line::styled(copy, Style::default().fg(Color::White))),
                Rect::new(
                    copy_x,
                    area.y.saturating_add(4 + offset as u16),
                    copy_width,
                    1,
                ),
            );
        }
        return;
    }

    for (offset, mark) in ["▟█▙▟█▙", "██████", "██▛▜██"].into_iter().enumerate()
    {
        frame.render_widget(
            Paragraph::new(Line::styled(mark, Style::default().fg(accent())))
                .alignment(ratatui::layout::Alignment::Center),
            Rect::new(
                inner.x,
                area.y.saturating_add(4 + offset as u16),
                inner.width,
                1,
            ),
        );
    }

    let text_width = inner.width.saturating_sub(2);
    let text_x = inner.x.saturating_add(1);
    frame.render_widget(
        Paragraph::new(Line::styled(
            ">_ CODE · BUILD · DEPLOY",
            Style::default().fg(Color::White),
        ))
        .alignment(ratatui::layout::Alignment::Center),
        Rect::new(text_x, area.y.saturating_add(8), text_width, 1),
    );
    let wrapped = area.width < 40;
    let copy_height = if wrapped { 2 } else { 1 };
    frame.render_widget(
        Paragraph::new(MASTHEAD_DESCRIPTION)
            .style(Style::default().fg(Color::White))
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(Wrap { trim: true }),
        Rect::new(text_x, area.y.saturating_add(9), text_width, copy_height),
    );
    frame.render_widget(
        Paragraph::new(MASTHEAD_PROMISE)
            .style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(Wrap { trim: true }),
        Rect::new(
            text_x,
            area.y.saturating_add(9 + copy_height),
            text_width,
            copy_height,
        ),
    );
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

fn workspace_removal_rect(area: Rect) -> Rect {
    centered_fixed(54, 11, area)
}

fn dashboard_request(state_dir: &Path, request: Request) -> Result<serde_json::Value> {
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
    state_dir: &Path,
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

fn compact_json_message(response: &serde_json::Value, fallback: &str) -> String {
    if let Some(message) = response.get("message").and_then(serde_json::Value::as_str) {
        return message.to_owned();
    }
    if let Some(pane_id) = response.get("pane_id").and_then(serde_json::Value::as_str) {
        return format!("{fallback}: {}", &pane_id[..pane_id.len().min(8)]);
    }
    fallback.to_owned()
}

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
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn rendered_masthead(width: u16) -> String {
        let height = workspace_masthead_height(width);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_workspace_masthead(frame, frame.area()))
            .expect("draw masthead");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn workspace_masthead_reflows_without_shortening_its_identity() {
        for width in [48, 80] {
            let rendered = rendered_masthead(width);
            assert!(rendered.contains("▪ ▪ ▪"));
            assert!(rendered.contains("B A S T I O N"));
            assert!(rendered.contains("▟█▙▟█▙"));
            assert!(rendered.contains("██████"));
            assert!(rendered.contains("██▛▜██"));
            assert!(rendered.contains("CODE · BUILD · DEPLOY"));
            assert!(rendered.contains(MASTHEAD_DESCRIPTION));
            assert!(rendered.contains(MASTHEAD_PROMISE));
        }
        assert_eq!(workspace_masthead_height(80), 9);
        assert_eq!(workspace_masthead_height(48), 9);
        assert_eq!(workspace_masthead_height(47), 13);
        assert_eq!(workspace_masthead_height(36), 15);
    }

    fn rendered_workspace_dashboard(width: u16, height: u16) -> String {
        let tabs = vec![json!({ "name": "Main" }), json!({ "name": "Research" })];
        let panes = vec![
            json!({
                "pane_id": "pane-2",
                "label": "Pane 2",
                "agent_kind": "codex",
                "agent_state": "working",
                "health": "active",
                "command": "codex",
                "idle_seconds": 3
            }),
            json!({
                "pane_id": "pane-10",
                "label": "Pane 10",
                "agent_kind": "claude",
                "agent_state": "attention",
                "health": "active",
                "command": "claude",
                "idle_seconds": 12
            }),
        ];
        let resumable = vec![json!({ "agent_kind": "claude" })];
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_window_dashboard(
                    frame,
                    frame.area(),
                    DashboardView {
                        tabs: &tabs,
                        selected_tab: 0,
                        panes: &panes,
                        selected_pane: 0,
                        pane_offset: 0,
                        notice: "ready",
                        input: None,
                        manage_open: false,
                        pane_menu_selected: 0,
                        tab_menu_open: false,
                        tab_menu_selected: 0,
                        resume_picker_open: false,
                        resumable_slots: &resumable,
                        selected_resume: 0,
                        workspace: Path::new("/test/my-app"),
                    },
                )
            })
            .expect("draw dashboard");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn workspace_dashboard_uses_the_full_screen_and_reflows_details() {
        let portrait = Rect::new(0, 0, 48, 28);
        let portrait_areas = window_areas(dashboard_rect(portrait));
        assert_eq!(dashboard_rect(portrait), portrait);
        assert_eq!(portrait_areas.details.width, 0);
        assert_eq!(portrait_areas.identity.y, 0);
        assert_eq!(portrait_areas.notice.bottom(), portrait.bottom());

        let landscape = Rect::new(0, 0, 100, 28);
        let landscape_areas = window_areas(dashboard_rect(landscape));
        assert!(landscape_areas.details.width > 0);
        assert_eq!(landscape_areas.panes.x, 0);
        assert_eq!(landscape_areas.details.right(), landscape.right());
    }

    #[test]
    fn workspace_removal_confirmation_stays_compact_in_portrait() {
        let portrait = Rect::new(0, 0, 48, 40);
        let dialog = workspace_removal_rect(portrait);
        assert_eq!((dialog.width, dialog.height), (46, 11));
        assert!(dialog.height < portrait.height / 2);
        assert_eq!(dialog.bottom().saturating_sub(2), dialog.y + 9);

        let landscape = Rect::new(0, 0, 100, 28);
        let dialog = workspace_removal_rect(landscape);
        assert_eq!((dialog.width, dialog.height), (54, 11));
    }

    #[test]
    fn workspace_dashboard_renders_touch_targets_in_portrait_and_landscape() {
        for (width, height) in [(48, 28), (100, 28)] {
            let rendered = rendered_workspace_dashboard(width, height);
            assert!(rendered.contains("▪ ▪ ▪"));
            assert!(rendered.contains("MY-APP"));
            assert!(rendered.contains("TABS"));
            assert!(rendered.contains("Pane 2"));
            assert!(rendered.contains("codex · working"));
            assert!(rendered.contains("+ PANE"));
            assert!(rendered.contains("RESUME"));
            assert!(rendered.contains("WORKSPACES"));
        }
        assert!(rendered_workspace_dashboard(100, 28).contains("PANE DETAILS"));
        assert!(!rendered_workspace_dashboard(48, 28).contains("PANE DETAILS"));
    }

    #[test]
    fn dashboard_hit_areas_and_touch_scroll_are_bounded() {
        let screen = Rect::new(0, 0, 48, 28);
        let tab_menu = tab_menu_rect(screen);
        let pane_menu = pane_menu_rect(screen);
        let one_session = resume_menu_rect(screen, 1);
        let many_sessions = resume_menu_rect(screen, 20);
        assert_eq!((tab_menu.width, tab_menu.height), (38, 8));
        assert_eq!((pane_menu.width, pane_menu.height), (40, 9));
        assert_eq!((one_session.width, one_session.height), (44, 8));
        assert_eq!((many_sessions.width, many_sessions.height), (44, 12));
        assert!(many_sessions.height < screen.height / 2);
        assert!(contains(tab_menu, tab_menu.x + 1, tab_menu.y + 4));
        assert!(!contains(tab_menu, tab_menu.x - 1, tab_menu.y + 4));

        assert_eq!(resume_menu_viewport(0, 10, many_sessions), (0, 5));
        assert_eq!(resume_menu_viewport(7, 10, many_sessions), (3, 5));
        assert_eq!(resume_menu_viewport(9, 10, many_sessions), (5, 5));

        let tabs = dashboard_tab_areas(Rect::new(0, 4, 48, 3), 3, 1);
        assert!(tabs.compact);
        assert!(!contains(tabs.previous, tabs.add.x, tabs.add.y));
        assert!(!contains(tabs.next, tabs.manage.x, tabs.manage.y));

        let actions = dashboard_action_areas(Rect::new(0, 25, 48, 3));
        assert!(actions[0].right() <= actions[1].x);
        assert!(actions[1].right() <= actions[2].x);

        assert_eq!(scrolled_pane_selection(0, 3, true), 0);
        assert_eq!(scrolled_pane_selection(0, 3, false), 1);
        assert_eq!(scrolled_pane_selection(2, 3, false), 2);
        assert_eq!(scrolled_pane_selection(0, 0, false), 0);
    }

    #[test]
    fn long_tab_names_are_truncated_for_display_only() {
        assert_eq!(
            truncate_display_label("Research and planning", 10),
            "Research …"
        );
        assert_eq!(truncate_display_label("界面设计工作区", 7), "界面设…");
        assert_eq!(truncate_display_label("Main", 10), "Main");
        assert_eq!(truncate_display_label("Main", 1), "…");
        assert_eq!(truncate_display_label("Main", 0), "");
    }

    fn test_pane(id: &str, label: &str) -> PaneDescriptor {
        PaneDescriptor {
            pane_id: id.to_owned(),
            label: label.to_owned(),
            agent: "claude".to_owned(),
            state: "working".to_owned(),
            tab: "Main".to_owned(),
            workspace_root: "/test/my-app".to_owned(),
            resume_command: None,
        }
    }

    #[test]
    fn attached_pane_layout_preserves_portrait_width_and_adds_a_wide_rail() {
        for area in [
            Rect::new(0, 0, 40, 30),
            Rect::new(0, 0, 48, 30),
            Rect::new(0, 0, 86, 24),
        ] {
            let layout = pane_layout(area);
            assert_eq!(layout.header.height, 2);
            assert_eq!(layout.terminal.width, area.width);
            assert_eq!(layout.terminal.height, area.height - 2);
            assert_eq!(layout.rail.width, 0);
        }

        let layout = pane_layout(Rect::new(0, 0, 100, 28));
        assert_eq!(layout.terminal.width, 75);
        assert_eq!(layout.rail.width, 25);
        assert_eq!(layout.terminal.right(), layout.rail.x);
        assert_eq!(layout.rail.right(), 100);

        let layout = pane_layout(Rect::new(0, 0, 120, 32));
        assert_eq!(layout.terminal.width, 90);
        assert_eq!(layout.rail.width, 30);

        for width in [24, 40, 48, 100] {
            let header = Rect::new(0, 0, width, 2);
            let areas = pane_header_areas(header);
            assert_eq!(areas.workspace.x, header.x);
            assert_eq!(areas.workspace.right(), areas.action.x);
            assert_eq!(areas.action.right(), header.right());
            assert_eq!(areas.status.x, header.x);
            assert_eq!(areas.status.width, header.width);
            assert_eq!(areas.status.y, header.y + 1);
        }

        let mut pane = test_pane("one", "Pane 1");
        pane.state = "unknown".to_owned();
        assert_eq!(pane_header_status(&pane, false, 0), "Pane 1 · claude");
        assert_eq!(
            pane_header_status(&pane, false, 54),
            "HISTORY · 54 lines up"
        );

        let offline_action = offline_pane_action_rect(Rect::new(0, 0, 48, 30));
        let offline = offline_pane_rect(Rect::new(0, 0, 48, 30));
        assert!(contains(offline, offline_action.x, offline_action.y));
        assert_eq!(offline_action.y, offline.bottom() - 2);
    }

    #[test]
    fn attached_pane_roster_cycles_every_pane_and_menus_stay_compact() {
        let roster = vec![
            test_pane("one", "Pane 1"),
            test_pane("two", "Pane 2"),
            test_pane("three", "Pane 3"),
        ];
        assert_eq!(next_pane_id(&roster, "one").as_deref(), Some("two"));
        assert_eq!(next_pane_id(&roster, "two").as_deref(), Some("three"));
        assert_eq!(next_pane_id(&roster, "three").as_deref(), Some("one"));
        assert_eq!(next_pane_id(&roster[..1], "one"), None);

        let screen = Rect::new(0, 0, 48, 30);
        let menu = pane_overlay_rect(screen, &PaneOverlay::Menu { selected: 0 }, roster.len());
        assert_eq!((menu.width, menu.height), (44, 11));
        let switch = pane_overlay_rect(screen, &PaneOverlay::Switch { selected: 0 }, roster.len());
        assert_eq!((switch.width, switch.height), (44, 7));
        assert!(menu.height < screen.height / 2);
    }

    #[test]
    fn modal_menus_ignore_letters_and_accept_only_explicit_navigation() {
        for character in ['b', 'r', 'd', 's', 'j', 'k', 'x'] {
            assert_eq!(
                modal_navigation(&KeyCode::Char(character)),
                ModalNavigation::Ignore
            );
        }
        assert_eq!(modal_navigation(&KeyCode::Esc), ModalNavigation::Cancel);
        assert_eq!(modal_navigation(&KeyCode::Up), ModalNavigation::Previous);
        assert_eq!(modal_navigation(&KeyCode::Down), ModalNavigation::Next);
        assert_eq!(modal_navigation(&KeyCode::Enter), ModalNavigation::Confirm);

        let (acknowledged_tx, acknowledged_rx) = mpsc::channel();
        let touch = PaneMouseInput {
            event: MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 47,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            acknowledged: Some(acknowledged_tx),
        };
        drop(touch);
        assert!(
            acknowledged_rx
                .recv_timeout(Duration::from_millis(20))
                .is_ok(),
            "dropping a routed touch must unblock the input pump"
        );

        let mut terminal = workspace_terminal::Terminal::new(2, 8);
        terminal.process(b"prompt");
        let mut prior_screen = Some(terminal.snapshot());
        let mut retained_frame = Some(Buffer::empty(Rect::new(0, 0, 8, 2)));
        invalidate_pane_frame(&mut retained_frame, &mut prior_screen);
        assert!(retained_frame.is_none());
        assert!(prior_screen.is_none());
    }

    #[test]
    fn panes_sort_naturally_and_keep_the_selected_identity() {
        let mut panes = vec![
            json!({ "pane_id": "ten", "label": "Pane 10" }),
            json!({ "pane_id": "custom", "label": "Research" }),
            json!({ "pane_id": "two", "label": "Pane 2" }),
            json!({ "pane_id": "one", "label": "Pane 1" }),
        ];
        panes.sort_by_key(pane_order_key);
        assert_eq!(
            panes.iter().map(pane_label).collect::<Vec<_>>(),
            vec!["Pane 1", "Pane 2", "Pane 10", "Research"]
        );

        let mut selected = HashMap::new();
        remember_selected_pane(&mut selected, "Main", &panes, 1);
        assert_eq!(selected.get("Main").map(String::as_str), Some("two"));
    }

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
