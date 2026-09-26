use crate::{UpdateCommand, daemon_status, start_daemon, stop_daemon};
use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const REPOSITORY: &str = "samperez10/bastion";
const ARCHIVE: &str = "bastion-termux-aarch64.tar.gz";
const CHECKSUM: &str = "bastion-termux-aarch64.tar.gz.sha256";
const ROLLBACK_DIR: &str = "update-rollback";
const ROLLBACK_METADATA: &str = "metadata.json";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const BINARIES: [&str; 4] = [
    "bastion",
    "workspace-daemon",
    "termux-tui",
    "workspace-agent",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UpdateCache {
    pub checked_at: u64,
    pub current_version: String,
    pub release: Option<Release>,
    pub skipped_version: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Release {
    pub version: String,
    pub tag: String,
    pub archive_url: String,
    pub checksum_url: String,
    #[serde(default)]
    pub archive_size: u64,
    #[serde(default)]
    pub checksum_size: u64,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RollbackMetadata {
    version: String,
    created_at: u64,
}

struct StagingDir {
    path: PathBuf,
}

struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)?;
        let path = state_dir.join("update.lock");
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let active = fs::read_to_string(&path)
                        .ok()
                        .and_then(|value| value.trim().parse::<i32>().ok())
                        .is_some_and(process_is_alive);
                    if active {
                        anyhow::bail!("another Bastion update is already running");
                    }
                    fs::remove_file(&path).context("remove stale update lock")?;
                }
                Err(error) => return Err(error).context("create update lock"),
            }
        }
        anyhow::bail!("could not acquire the Bastion update lock")
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl StagingDir {
    fn create(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)?;
        cleanup_stale_staging(state_dir);
        let path = state_dir.join(format!(
            "update-staging-{}-{}",
            std::process::id(),
            unix_time()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).context("remove stale update staging directory")?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct PartialDownload {
    path: PathBuf,
    committed: bool,
}

impl PartialDownload {
    fn new(destination: &Path) -> Result<Self> {
        let path = destination.with_extension("part");
        if path.exists() {
            fs::remove_file(&path).context("remove stale partial download")?;
        }
        Ok(Self {
            path,
            committed: false,
        })
    }

    fn commit(mut self, destination: &Path) -> Result<()> {
        fs::rename(&self.path, destination).context("commit completed download")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartialDownload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn command(state_dir: &Path, command: UpdateCommand) -> Result<()> {
    match command {
        UpdateCommand::Check { force: _, quiet } => {
            // Explicit checks are always fresh. The 24-hour limit is enforced
            // before refresh_in_background spawns this command.
            match check(state_dir, true) {
                Ok(cache) => {
                    if !quiet {
                        print_status(&cache);
                    }
                }
                Err(error) => {
                    if !quiet {
                        print_offline_error(&error);
                    }
                }
            }
            Ok(())
        }
        UpdateCommand::Status => {
            print_status(&load_cache(state_dir).unwrap_or_default());
            Ok(())
        }
        UpdateCommand::Install { yes } => install(state_dir, yes),
        UpdateCommand::Rollback { yes } => rollback(state_dir, yes),
        UpdateCommand::Skip => skip(state_dir),
    }
}

pub(crate) fn refresh_in_background(state_dir: &Path) {
    let mut cache = load_cache(state_dir).unwrap_or_default();
    if !is_due(&cache) {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    // Reserve this check window before spawning so a very fast child cannot
    // write a result that the parent subsequently overwrites.
    cache.checked_at = unix_time();
    cache.current_version = env!("CARGO_PKG_VERSION").to_owned();
    if save_cache(state_dir, &cache).is_err() {
        return;
    }
    if Command::new(executable)
        .args([
            "--state-dir",
            state_dir.to_string_lossy().as_ref(),
            "update",
            "check",
            "--force",
            "--quiet",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_err()
    {
        cache.checked_at = 0;
        let _ = save_cache(state_dir, &cache);
    }
}

fn check(state_dir: &Path, force: bool) -> Result<UpdateCache> {
    let mut cache = load_cache(state_dir).unwrap_or_default();
    if !force && !is_due(&cache) {
        return Ok(cache);
    }
    cache.checked_at = unix_time();
    cache.current_version = env!("CARGO_PKG_VERSION").to_owned();
    match fetch_release() {
        Ok(release) => {
            cache.release = Some(release);
            cache.last_error = None;
        }
        Err(error) => {
            cache.last_error = Some(format!("{error:#}"));
            save_cache(state_dir, &cache)?;
            return Err(error);
        }
    }
    save_cache(state_dir, &cache)?;
    Ok(cache)
}

fn fetch_release() -> Result<Release> {
    let repository =
        std::env::var("BASTION_UPDATE_REPOSITORY").unwrap_or_else(|_| REPOSITORY.to_owned());
    let url = format!("https://api.github.com/repos/{repository}/releases?per_page=10");
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "2",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            &url,
        ])
        .output()
        .context("run curl to check for Bastion updates")?;
    if !output.status.success() {
        anyhow::bail!("GitHub release check failed: {}", output.status);
    }
    let releases: Vec<GithubRelease> =
        serde_json::from_slice(&output.stdout).context("parse GitHub release response")?;
    releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = release.tag_name.strip_prefix('v')?.to_owned();
            let parsed = Version::parse(&version).ok()?;
            let archive_url = release
                .assets
                .iter()
                .find(|asset| asset.name == ARCHIVE)?
                .browser_download_url
                .clone();
            let archive_size = release
                .assets
                .iter()
                .find(|asset| asset.name == ARCHIVE)?
                .size;
            let checksum_url = release
                .assets
                .iter()
                .find(|asset| asset.name == CHECKSUM)?
                .browser_download_url
                .clone();
            let checksum_size = release
                .assets
                .iter()
                .find(|asset| asset.name == CHECKSUM)?
                .size;
            Some((
                parsed,
                Release {
                    version,
                    tag: release.tag_name,
                    archive_url,
                    checksum_url,
                    archive_size,
                    checksum_size,
                },
            ))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, release)| release)
        .context("no compatible Bastion release was found")
}

fn install(state_dir: &Path, yes: bool) -> Result<()> {
    ensure_installed_layout()?;
    let _lock = UpdateLock::acquire(state_dir)?;
    let cache = check(state_dir, true).context(
        "Could not reach GitHub to check for updates. Bastion was not changed; try again when connected",
    )?;
    let Some(release) = cache.release.filter(|release| is_newer(&release.version)) else {
        println!(
            "Bastion {} is already up to date.",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    };
    if !yes && !confirm(&release.version)? {
        println!("Update cancelled.");
        return Ok(());
    }

    let staging = StagingDir::create(state_dir)?;
    fs::create_dir_all(staging.path.join("payload"))?;
    install_from_release(state_dir, &staging.path, &release)
}

fn install_from_release(state_dir: &Path, staging: &Path, release: &Release) -> Result<()> {
    let archive = staging.join(ARCHIVE);
    let checksum = staging.join(CHECKSUM);
    download(
        &release.archive_url,
        &archive,
        "Downloading update",
        release.archive_size,
    )?;
    download(
        &release.checksum_url,
        &checksum,
        "Downloading checksum",
        release.checksum_size,
    )?;
    verify_checksum(&archive, &checksum)?;

    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(&archive)
        .arg("-C")
        .arg(staging.join("payload"))
        .status()
        .context("extract Bastion release")?;
    if !status.success() {
        anyhow::bail!("release extraction failed: {status}");
    }
    let package = staging.join("payload/bastion-termux-aarch64/bin");
    validate_payload(&package, &release.version)?;

    let destination = installed_bin_dir()?;
    let backup = staging.join("backup");
    fs::create_dir_all(&backup)?;
    for binary in BINARIES {
        let target = destination.join(binary);
        if target.is_file() {
            fs::copy(&target, backup.join(binary))
                .with_context(|| format!("back up {}", target.display()))?;
        }
    }
    write_rollback_snapshot(state_dir, &backup, env!("CARGO_PKG_VERSION"))?;

    let daemon_was_running = daemon_status(&state_dir.to_path_buf()).is_some();
    let daemon_workspace = daemon_was_running
        .then(|| current_workspace(state_dir))
        .transpose()?;
    let stopped = if daemon_was_running {
        stop_daemon(&state_dir.to_path_buf())?
    } else {
        0
    };
    if let Err(error) = activate(&package, &destination) {
        let rollback_error = restore(&backup, &destination).err();
        if let Some(rollback_error) = rollback_error {
            return Err(error).context(format!("rollback also failed: {rollback_error:#}"));
        }
        if let Some(workspace) = daemon_workspace.as_ref()
            && let Err(restart_error) = start_daemon(&state_dir.to_path_buf(), workspace)
        {
            return Err(error).context(format!(
                "update activation failed; previous binaries were restored but their daemon did not restart: {restart_error:#}"
            ));
        }
        return Err(error).context("update activation failed; previous binaries were restored");
    }

    if let Some(workspace) = daemon_workspace.as_ref() {
        if let Err(error) = start_daemon(&state_dir.to_path_buf(), workspace) {
            restore(&backup, &destination)
                .context("new daemon failed and previous binaries could not be restored")?;
            if let Err(restart_error) = start_daemon(&state_dir.to_path_buf(), workspace) {
                return Err(error).context(format!(
                    "new daemon failed; previous binaries were restored but their daemon did not restart: {restart_error:#}"
                ));
            }
            return Err(error)
                .context("new daemon failed to start; previous binaries were restored");
        }
    }
    println!("Updated Bastion to {}.", release.version);
    if stopped > 0 {
        println!(
            "Restarted the daemon; {stopped} pane(s) were closed and saved agent sessions can resume."
        );
    }
    println!("Restart Bastion to use the new interface.");
    Ok(())
}

fn activate(package: &Path, destination: &Path) -> Result<()> {
    for binary in BINARIES {
        let target = destination.join(binary);
        let temporary = destination.join(format!(".{binary}.bastion-update"));
        fs::copy(package.join(binary), &temporary).with_context(|| format!("stage {binary}"))?;
        if let Err(error) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("activate {binary}"));
        }
    }
    Ok(())
}

fn restore(backup: &Path, destination: &Path) -> Result<()> {
    for binary in BINARIES {
        let source = backup.join(binary);
        if source.is_file() {
            let temporary = destination.join(format!(".{binary}.bastion-rollback"));
            fs::copy(&source, &temporary).with_context(|| format!("stage rollback {binary}"))?;
            fs::rename(&temporary, destination.join(binary))
                .with_context(|| format!("restore {binary}"))?;
        }
    }
    Ok(())
}

fn rollback(state_dir: &Path, yes: bool) -> Result<()> {
    ensure_installed_layout()?;
    let _lock = UpdateLock::acquire(state_dir)?;
    let rollback_root = state_dir.join(ROLLBACK_DIR);
    let metadata = load_rollback_metadata(&rollback_root)
        .context("No rollback is available yet. Install at least one Bastion update first")?;
    if metadata.version == env!("CARGO_PKG_VERSION") {
        anyhow::bail!("No earlier Bastion release is available to restore");
    }
    let package = rollback_root.join("bin");
    validate_payload(&package, &metadata.version)
        .context("the local rollback snapshot is incomplete or invalid")?;
    if !yes && !confirm_rollback(&metadata.version)? {
        println!("Rollback cancelled.");
        return Ok(());
    }

    let staging = StagingDir::create(state_dir)?;
    let current = staging.path.join("current");
    fs::create_dir_all(&current)?;
    let destination = installed_bin_dir()?;
    copy_binaries(&destination, &current)?;

    let daemon_was_running = daemon_status(&state_dir.to_path_buf()).is_some();
    let daemon_workspace = daemon_was_running
        .then(|| current_workspace(state_dir))
        .transpose()?;
    let stopped = if daemon_was_running {
        stop_daemon(&state_dir.to_path_buf())?
    } else {
        0
    };

    if let Err(error) = activate(&package, &destination) {
        let restore_error = restore(&current, &destination).err();
        if let Some(restore_error) = restore_error {
            return Err(error).context(format!(
                "rollback activation failed and the current release could not be restored: {restore_error:#}"
            ));
        }
        if let Some(workspace) = daemon_workspace.as_ref()
            && let Err(restart_error) = start_daemon(&state_dir.to_path_buf(), workspace)
        {
            return Err(error).context(format!(
                "rollback failed; current binaries were restored but their daemon did not restart: {restart_error:#}"
            ));
        }
        return Err(error).context("rollback failed; the current release was restored");
    }

    if let Some(workspace) = daemon_workspace.as_ref()
        && let Err(error) = start_daemon(&state_dir.to_path_buf(), workspace)
    {
        restore(&current, &destination)
            .context("rolled-back daemon failed and the current release could not be restored")?;
        if let Err(restart_error) = start_daemon(&state_dir.to_path_buf(), workspace) {
            return Err(error).context(format!(
                "rolled-back daemon failed; current binaries were restored but their daemon did not restart: {restart_error:#}"
            ));
        }
        return Err(error).context("rolled-back daemon failed to start; current release restored");
    }

    if let Err(error) = write_rollback_snapshot(state_dir, &current, env!("CARGO_PKG_VERSION")) {
        if daemon_was_running {
            let _ = stop_daemon(&state_dir.to_path_buf());
        }
        restore(&current, &destination)
            .context("could not rotate rollback backup or restore the current release")?;
        if let Some(workspace) = daemon_workspace.as_ref()
            && let Err(restart_error) = start_daemon(&state_dir.to_path_buf(), workspace)
        {
            return Err(error).context(format!(
                "could not preserve the current release; rollback was undone but its daemon did not restart: {restart_error:#}"
            ));
        }
        return Err(error).context("could not preserve the current release; rollback was undone");
    }

    println!("Rolled Bastion back to {}.", metadata.version);
    if stopped > 0 {
        println!(
            "Restarted the daemon; {stopped} pane(s) were closed and saved agent sessions can resume."
        );
    }
    println!("Run `bastion update rollback` again to restore the version you just replaced.");
    Ok(())
}

fn copy_binaries(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for binary in BINARIES {
        let source = source.join(binary);
        if !source.is_file() {
            anyhow::bail!("binary snapshot is missing {binary}");
        }
        fs::copy(&source, destination.join(binary))
            .with_context(|| format!("copy {}", source.display()))?;
    }
    Ok(())
}

fn write_rollback_snapshot(state_dir: &Path, source: &Path, version: &str) -> Result<()> {
    let destination = state_dir.join(ROLLBACK_DIR);
    let next = state_dir.join(format!("{ROLLBACK_DIR}.next"));
    let previous = state_dir.join(format!("{ROLLBACK_DIR}.previous"));
    if next.exists() {
        fs::remove_dir_all(&next).context("remove incomplete rollback snapshot")?;
    }
    fs::create_dir_all(next.join("bin"))?;
    if let Err(error) = copy_binaries(source, &next.join("bin")).and_then(|_| {
        let metadata = RollbackMetadata {
            version: version.to_owned(),
            created_at: unix_time(),
        };
        fs::write(
            next.join(ROLLBACK_METADATA),
            serde_json::to_vec_pretty(&metadata)?,
        )?;
        Ok(())
    }) {
        let _ = fs::remove_dir_all(&next);
        return Err(error).context("prepare local rollback snapshot");
    }

    if previous.exists() {
        fs::remove_dir_all(&previous).context("remove obsolete rollback snapshot")?;
    }
    if destination.exists() {
        fs::rename(&destination, &previous).context("preserve previous rollback snapshot")?;
    }
    if let Err(error) = fs::rename(&next, &destination) {
        if previous.exists() {
            let _ = fs::rename(&previous, &destination);
        }
        return Err(error).context("activate local rollback snapshot");
    }
    if previous.exists() {
        fs::remove_dir_all(&previous).context("remove replaced rollback snapshot")?;
    }
    Ok(())
}

fn load_rollback_metadata(root: &Path) -> Result<RollbackMetadata> {
    let value = fs::read(root.join(ROLLBACK_METADATA))?;
    serde_json::from_slice(&value).context("parse rollback metadata")
}

fn validate_payload(package: &Path, expected_version: &str) -> Result<()> {
    for binary in BINARIES {
        let path = package.join(binary);
        if !path.is_file() {
            anyhow::bail!("release is missing {binary}");
        }
    }
    let output = Command::new(package.join("bastion"))
        .arg("--version")
        .output()
        .context("run downloaded Bastion executable")?;
    let version = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || !version
            .split_whitespace()
            .any(|part| part == expected_version)
    {
        anyhow::bail!("downloaded Bastion version does not match {expected_version}");
    }
    for binary in BINARIES.into_iter().filter(|binary| *binary != "bastion") {
        let status = Command::new(package.join(binary))
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("run downloaded {binary} executable"))?;
        if !status.success() {
            anyhow::bail!("downloaded {binary} executable failed validation");
        }
    }
    Ok(())
}

fn download(url: &str, destination: &Path, label: &str, total: u64) -> Result<()> {
    let interactive = io::stdout().is_terminal();
    if !interactive {
        println!("{label}…");
    }
    let partial = PartialDownload::new(destination)?;
    let mut child = Command::new("curl")
        .args([
            "-fsSL",
            "--remove-on-error",
            "--retry",
            "3",
            "--connect-timeout",
            "10",
            "-o",
        ])
        .arg(&partial.path)
        .arg(url)
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("download {label}"))?;
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let mut frame = 0_usize;
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for curl download")? {
            break status;
        }
        if interactive {
            let downloaded = fs::metadata(&partial.path)
                .map(|meta| meta.len())
                .unwrap_or(0);
            draw_download_progress(frames[frame % frames.len()], label, downloaded, total)?;
            frame += 1;
        }
        thread::sleep(Duration::from_millis(90));
    };
    let mut error = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut error);
    }
    if !status.success() {
        if interactive {
            clear_progress_line()?;
        }
        let detail = error.trim();
        if detail.is_empty() {
            anyhow::bail!("{label} failed: {status}");
        }
        anyhow::bail!("{label} failed: {detail}");
    }
    let downloaded = fs::metadata(&partial.path)
        .map(|meta| meta.len())
        .unwrap_or(total);
    partial.commit(destination)?;
    if interactive {
        clear_progress_line()?;
    }
    println!(
        "✓ {} · {}",
        completed_label(label),
        format_bytes(downloaded)
    );
    Ok(())
}

fn draw_download_progress(frame: &str, label: &str, downloaded: u64, total: u64) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "\r\x1b[2K{frame} {label}")?;
    if total > 0 {
        let percent = downloaded
            .saturating_mul(100)
            .min(total.saturating_mul(100))
            / total;
        write!(
            stdout,
            "  {percent:>3}% · {} / {}",
            format_bytes(downloaded),
            format_bytes(total)
        )?;
    } else if downloaded > 0 {
        write!(stdout, " · {}", format_bytes(downloaded))?;
    }
    stdout.flush()?;
    Ok(())
}

fn clear_progress_line() -> Result<()> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "\r\x1b[2K")?;
    stdout.flush()?;
    Ok(())
}

fn completed_label(label: &str) -> &str {
    match label {
        "Downloading update" => "Download complete",
        "Downloading checksum" => "Checksum received",
        value => value,
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn verify_checksum(archive: &Path, checksum: &Path) -> Result<()> {
    let interactive = io::stdout().is_terminal();
    let expected = fs::read_to_string(checksum)?
        .split_whitespace()
        .next()
        .context("checksum file is empty")?
        .to_ascii_lowercase();
    if expected.len() != 64
        || !expected
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        anyhow::bail!("release checksum is invalid");
    }
    if interactive {
        print!("⠋ Verifying package…");
        io::stdout().flush()?;
    } else {
        println!("Verifying package…");
    }
    let output = Command::new("sha256sum")
        .arg(archive)
        .output()
        .context("calculate release checksum")?;
    if !output.status.success() {
        if interactive {
            clear_progress_line()?;
        }
        anyhow::bail!("sha256sum failed: {}", output.status);
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if actual != expected {
        if interactive {
            clear_progress_line()?;
        }
        anyhow::bail!("release checksum verification failed");
    }
    if interactive {
        clear_progress_line()?;
    }
    println!("✓ Package verified");
    Ok(())
}

fn skip(state_dir: &Path) -> Result<()> {
    let mut cache = check(state_dir, false)?;
    let release = cache
        .release
        .as_ref()
        .context("no release has been checked")?;
    if !is_newer(&release.version) {
        anyhow::bail!("Bastion is already up to date");
    }
    cache.skipped_version = Some(release.version.clone());
    save_cache(state_dir, &cache)?;
    println!("Skipped Bastion {}.", release.version);
    Ok(())
}

fn print_status(cache: &UpdateCache) {
    let current = env!("CARGO_PKG_VERSION");
    match cache.release.as_ref() {
        Some(release) if is_newer(&release.version) => {
            if cache.skipped_version.as_deref() == Some(release.version.as_str()) {
                println!("Bastion {current} · {} skipped", release.version);
            } else {
                println!("Update available: {current} → {}", release.version);
                println!("Run `bastion update install` to install it.");
            }
        }
        _ => println!("Bastion {current} is up to date."),
    }
}

fn print_offline_error(error: &anyhow::Error) {
    println!("Could not check GitHub for Bastion updates.");
    println!("Bastion remains available offline; try again when connected.");
    println!("Details: {error:#}");
}

fn confirm(version: &str) -> Result<bool> {
    print!("Install Bastion {version} and restart its daemon if running? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn confirm_rollback(version: &str) -> Result<bool> {
    print!("Restore Bastion {version} from the local rollback snapshot? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn ensure_installed_layout() -> Result<()> {
    let executable = std::env::current_exe()?;
    let prefix = std::env::var_os("PREFIX").map(PathBuf::from);
    let expected = prefix.map(|prefix| prefix.join("bin"));
    if executable.parent() != expected.as_deref() {
        anyhow::bail!("self-update is available only for a release installed in $PREFIX/bin");
    }
    let directory = executable
        .parent()
        .context("resolve Bastion installation")?;
    for binary in BINARIES {
        if !directory.join(binary).is_file() {
            anyhow::bail!("the Bastion installation is incomplete: missing {binary}");
        }
    }
    Ok(())
}

fn installed_bin_dir() -> Result<PathBuf> {
    std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .context("resolve installed Bastion directory")
}

fn current_workspace(state_dir: &Path) -> Result<PathBuf> {
    Ok(workspace_core::StateDb::open(state_dir)?
        .focused_project()?
        .map(|project| PathBuf::from(project.canonical_root))
        .unwrap_or(std::env::current_dir()?))
}

fn is_newer(version: &str) -> bool {
    let Ok(latest) = Version::parse(version) else {
        return false;
    };
    Version::parse(env!("CARGO_PKG_VERSION")).is_ok_and(|current| latest > current)
}

fn is_due(cache: &UpdateCache) -> bool {
    unix_time().saturating_sub(cache.checked_at) >= CHECK_INTERVAL.as_secs()
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn cleanup_stale_staging(state_dir: &Path) {
    let Ok(entries) = fs::read_dir(state_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("update-staging-")
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join("update.json")
}

fn load_cache(state_dir: &Path) -> Result<UpdateCache> {
    match fs::read_to_string(cache_path(state_dir)) {
        Ok(value) => serde_json::from_str(&value).context("parse Bastion update cache"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(UpdateCache::default()),
        Err(error) => Err(error).context("read Bastion update cache"),
    }
}

fn save_cache(state_dir: &Path, cache: &UpdateCache) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = cache_path(state_dir);
    let temporary = state_dir.join("update.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(cache)?)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bastion-update-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_fake_binaries(directory: &Path, marker: &str) {
        fs::create_dir_all(directory).unwrap();
        for binary in BINARIES {
            fs::write(directory.join(binary), format!("{binary}-{marker}")).unwrap();
        }
    }

    #[test]
    fn compares_stable_and_prerelease_versions() {
        assert!(is_newer("0.1.0-alpha.10"));
        assert!(is_newer("0.1.0"));
        assert!(!is_newer("0.1.0-alpha.9"));
        assert!(!is_newer("not-a-version"));
    }

    #[test]
    fn check_interval_uses_cached_timestamp() {
        let mut cache = UpdateCache {
            checked_at: unix_time(),
            ..UpdateCache::default()
        };
        assert!(!is_due(&cache));
        cache.checked_at = 0;
        assert!(is_due(&cache));
    }

    #[test]
    fn formats_progress_sizes_for_people() {
        assert_eq!(format_bytes(96), "96 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn partial_download_is_removed_until_committed() {
        let root = TestDir::new("partial");
        let destination = root.0.join("release.tar.gz");
        let partial = PartialDownload::new(&destination).unwrap();
        fs::write(&partial.path, b"incomplete").unwrap();
        let partial_path = partial.path.clone();
        drop(partial);
        assert!(!partial_path.exists());
        assert!(!destination.exists());

        let partial = PartialDownload::new(&destination).unwrap();
        fs::write(&partial.path, b"complete").unwrap();
        partial.commit(&destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"complete");
    }

    #[test]
    fn rejects_a_mismatched_release_checksum() {
        let root = TestDir::new("checksum");
        let archive = root.0.join("archive");
        let checksum = root.0.join("archive.sha256");
        fs::write(&archive, b"real package").unwrap();
        fs::write(&checksum, format!("{}  archive\n", "0".repeat(64))).unwrap();
        let error = verify_checksum(&archive, &checksum).unwrap_err();
        assert!(format!("{error:#}").contains("checksum verification failed"));
    }

    #[test]
    fn rollback_snapshot_is_durable_and_rotates_atomically() {
        let root = TestDir::new("rollback");
        let first = root.0.join("first");
        let second = root.0.join("second");
        write_fake_binaries(&first, "one");
        write_fake_binaries(&second, "two");

        write_rollback_snapshot(&root.0, &first, "1.0.0").unwrap();
        let metadata = load_rollback_metadata(&root.0.join(ROLLBACK_DIR)).unwrap();
        assert_eq!(metadata.version, "1.0.0");
        assert_eq!(
            fs::read_to_string(root.0.join(ROLLBACK_DIR).join("bin/bastion")).unwrap(),
            "bastion-one"
        );

        write_rollback_snapshot(&root.0, &second, "2.0.0").unwrap();
        let metadata = load_rollback_metadata(&root.0.join(ROLLBACK_DIR)).unwrap();
        assert_eq!(metadata.version, "2.0.0");
        assert_eq!(
            fs::read_to_string(root.0.join(ROLLBACK_DIR).join("bin/bastion")).unwrap(),
            "bastion-two"
        );
        assert!(!root.0.join(format!("{ROLLBACK_DIR}.next")).exists());
        assert!(!root.0.join(format!("{ROLLBACK_DIR}.previous")).exists());
    }

    #[test]
    fn updater_lock_prevents_parallel_mutation_and_recovers_stale_locks() {
        let root = TestDir::new("lock");
        let lock = UpdateLock::acquire(&root.0).unwrap();
        assert!(UpdateLock::acquire(&root.0).is_err());
        drop(lock);
        fs::write(root.0.join("update.lock"), "99999999\n").unwrap();
        let recovered = UpdateLock::acquire(&root.0).unwrap();
        drop(recovered);
        assert!(!root.0.join("update.lock").exists());
    }

    #[test]
    fn staging_cleanup_removes_abandoned_downloads() {
        let root = TestDir::new("staging");
        let abandoned = root.0.join("update-staging-abandoned");
        fs::create_dir_all(&abandoned).unwrap();
        fs::write(abandoned.join("release.part"), b"partial").unwrap();
        let staging = StagingDir::create(&root.0).unwrap();
        assert!(!abandoned.exists());
        let active = staging.path.clone();
        drop(staging);
        assert!(!active.exists());
    }
}
