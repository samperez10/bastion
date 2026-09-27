//! A deliberately isolated compatibility lab for Bastion's live-pane renderer.
//!
//! This crate must not be linked from `termux-tui`.  An engine only graduates
//! after it compiles on Termux and passes the deterministic transcript probes
//! here; that prevents a renderer experiment from breaking the dashboard.

#[cfg(feature = "alacritty")]
use anyhow::Context;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[cfg(feature = "alacritty")]
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

#[derive(Debug, Parser)]
#[command(name = "terminal-lab", about = "Terminal-engine probes for Bastion")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Play an internal notification sound without exposing it in Bastion's CLI.
    Sound { kind: String },
    /// Print the engines this build can exercise.
    Engines,
    /// Run deterministic parser cases against the Alacritty terminal engine.
    Alacritty,
    /// Run a real command in a PTY and render its captured screen with Alacritty.
    Pty {
        /// Maximum capture time before the child is stopped.
        #[arg(long, default_value_t = 6)]
        seconds: u64,
        /// PTY width in columns. Use a portrait-friendly value for phone tests.
        #[arg(long, default_value_t = 48)]
        cols: u16,
        /// PTY height in rows.
        #[arg(long, default_value_t = 28)]
        rows: u16,
        /// Text that must appear in the rendered screen (repeatable).
        #[arg(long)]
        expect: Vec<String>,
        /// Working directory passed to the temporary PTY child.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Send a key to the temporary PTY child before capture (repeatable).
        /// Supported values: enter, esc, tab, up, down, left, right, or text:<value>.
        #[arg(long = "key")]
        keys: Vec<String>,
        /// Wait for the CLI to draw before sending the requested key sequence.
        #[arg(long, default_value_t = 0)]
        key_delay_ms: u64,
        /// Command and arguments to run; place them after `--`.
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Sound { kind } => {
            let kind = match kind.as_str() {
                "done" => bastion_audio::AlertSound::Done,
                "attention" => bastion_audio::AlertSound::Attention,
                _ => bail!("sound must be `done` or `attention`"),
            };
            bastion_audio::play(kind)
        }
        Command::Engines => engines(),
        Command::Alacritty => alacritty(),
        Command::Pty {
            seconds,
            cols,
            rows,
            expect,
            cwd,
            keys,
            key_delay_ms,
            command,
        } => pty(
            seconds,
            cols,
            rows,
            &expect,
            cwd.as_deref(),
            &keys,
            key_delay_ms,
            &command,
        ),
    }
}

fn engines() -> Result<()> {
    println!("legacy baseline: vt100 (no longer used by Bastion)");

    #[cfg(feature = "alacritty")]
    println!("alacritty_terminal: enabled (parser + grid probe available)");

    #[cfg(not(feature = "alacritty"))]
    println!("alacritty_terminal: disabled; rebuild with --features alacritty");

    println!(
        "wezterm-term: deferred — upstream-only workspace crate, not a stable crates.io dependency"
    );
    Ok(())
}

#[cfg(feature = "alacritty")]
fn alacritty() -> Result<()> {
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    // `Term` implements `Perform`; this parser route is the same ANSI layer
    // used by Alacritty's PTY event loop, but is deterministic and has no UI.
    let size = Size {
        lines: 4,
        columns: 20,
    };
    let mut term = new_term(&size, ReplySink::default());
    let mut parser = Processor::<StdSyncHandler>::new();
    parser.advance(&mut term, b"alpha\r\nbeta\x1b[2;8HOK\x1b[31m red\x1b[0m");

    let rows = screen_rows(&term, size.columns);

    let rendered = rows.join("\n");
    if !rendered.contains("alpha") || !rendered.contains("beta") || !rendered.contains("OK red") {
        bail!("Alacritty parser probe produced an unexpected grid: {rendered:?}");
    }

    println!("PASS alacritty_terminal parser/grid probe");
    println!("{rendered}");
    Ok(())
}

#[cfg(not(feature = "alacritty"))]
fn alacritty() -> Result<()> {
    bail!(
        "Alacritty probe is disabled; run cargo run -p terminal-lab --features alacritty -- alacritty"
    )
}

#[cfg(feature = "alacritty")]
#[derive(Clone, Copy)]
struct Size {
    lines: usize,
    columns: usize,
}

#[cfg(feature = "alacritty")]
impl alacritty_terminal::grid::Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Captures terminal replies (DA/DSR, keyboard-mode queries, and similar)
/// generated while parsing. The PTY probe writes them back to the child, which
/// is important for interactive agents that query terminal capabilities.
#[cfg(feature = "alacritty")]
#[derive(Clone, Default)]
struct ReplySink(Arc<Mutex<Vec<String>>>);

#[cfg(feature = "alacritty")]
impl alacritty_terminal::event::EventListener for ReplySink {
    fn send_event(&self, event: alacritty_terminal::event::Event) {
        if let alacritty_terminal::event::Event::PtyWrite(reply) = event {
            self.0.lock().unwrap().push(reply);
        }
    }
}

#[cfg(feature = "alacritty")]
impl ReplySink {
    fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[cfg(feature = "alacritty")]
fn new_term(size: &Size, replies: ReplySink) -> alacritty_terminal::term::Term<ReplySink> {
    alacritty_terminal::term::Term::new(alacritty_terminal::term::Config::default(), size, replies)
}

#[cfg(feature = "alacritty")]
fn screen_rows(term: &alacritty_terminal::term::Term<ReplySink>, columns: usize) -> Vec<String> {
    term.grid()
        .display_iter()
        .collect::<Vec<_>>()
        .chunks(columns)
        .map(|cells| {
            cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[cfg(feature = "alacritty")]
fn pty(
    seconds: u64,
    cols: u16,
    rows: u16,
    expect: &[String],
    cwd: Option<&std::path::Path>,
    keys: &[String],
    key_delay_ms: u64,
    command: &[String],
) -> Result<()> {
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    if cols == 0 || rows == 0 {
        bail!("--cols and --rows must be greater than zero");
    }
    let executable = command.first().context("a command is required")?;
    let mut child_command = CommandBuilder::new(executable);
    for argument in &command[1..] {
        child_command.arg(argument);
    }
    if let Some(cwd) = cwd {
        child_command.cwd(cwd);
    }
    child_command.env("TERM", "xterm-256color");
    child_command.env("COLORTERM", "truecolor");
    child_command.env("TERM_PROGRAM", "bastion-terminal-lab");

    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut child = pair.slave.spawn_command(child_command)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let _master = pair.master;
    let (output_tx, output_rx) = mpsc::channel();
    std::thread::spawn(move || copy_pty_output(reader, output_tx));

    let size = Size {
        lines: usize::from(rows),
        columns: usize::from(cols),
    };
    let replies = ReplySink::default();
    let mut term = new_term(&size, replies.clone());
    let mut parser = Processor::<StdSyncHandler>::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut raw_bytes = 0_usize;
    let mut reply_bytes = 0_usize;
    let mut child_exit = None;

    let key_at = Instant::now() + Duration::from_millis(key_delay_ms);
    let mut keys_sent = keys.is_empty();

    while Instant::now() < deadline {
        if !keys_sent && Instant::now() >= key_at {
            for key in keys {
                let bytes = key_bytes(key)?;
                writer.write_all(&bytes)?;
            }
            writer.flush()?;
            keys_sent = true;
        }
        match output_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                raw_bytes += bytes.len();
                parser.advance(&mut term, &bytes);
                for reply in replies.drain() {
                    reply_bytes += reply.len();
                    writer.write_all(reply.as_bytes())?;
                    writer.flush()?;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(status) = child.try_wait()? {
            child_exit = Some(status.exit_code());
            break;
        }
    }

    if child_exit.is_none() {
        child.kill().context("stop PTY probe child")?;
        child_exit = child.wait().ok().map(|status| status.exit_code());
    }

    let rendered = screen_rows(&term, size.columns).join("\n");
    for text in expect {
        if !rendered.contains(text) {
            bail!("expected {text:?} was absent from rendered PTY screen:\n{rendered}");
        }
    }

    println!("PASS Alacritty PTY probe: {}", command.join(" "));
    let cwd = cwd
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "child default".to_owned());
    println!(
        "size: {cols}x{rows} · cwd: {cwd} · raw: {raw_bytes} bytes · replies: {reply_bytes} bytes · exit: {child_exit:?}"
    );
    println!("--- rendered screen ---");
    println!("{rendered}");
    Ok(())
}

#[cfg(feature = "alacritty")]
fn key_bytes(key: &str) -> Result<Vec<u8>> {
    let bytes = match key {
        "enter" => b"\r".to_vec(),
        "esc" => b"\x1b".to_vec(),
        "tab" => b"\t".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        text if let Some(text) = text.strip_prefix("text:") => text.as_bytes().to_vec(),
        _ => bail!(
            "unsupported --key {key:?}; use enter, esc, tab, up, down, left, right, or text:<value>"
        ),
    };
    Ok(bytes)
}

#[cfg(feature = "alacritty")]
fn copy_pty_output(mut reader: Box<dyn Read + Send>, output_tx: mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0_u8; 4096];
    while let Ok(size) = reader.read(&mut buffer) {
        if size == 0 || output_tx.send(buffer[..size].to_vec()).is_err() {
            break;
        }
    }
}

#[cfg(not(feature = "alacritty"))]
#[allow(clippy::too_many_arguments)] // Keep parity with the feature-backed probe entry point.
fn pty(
    _: u64,
    _: u16,
    _: u16,
    _: &[String],
    _: Option<&std::path::Path>,
    _: &[String],
    _: u64,
    _: &[String],
) -> Result<()> {
    bail!("PTY probes require --features alacritty")
}
