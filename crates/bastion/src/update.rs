use crate::{UpdateCommand, daemon_status, start_daemon, stop_daemon};
use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const REPOSITORY: &str = "samperez10/bastion";
const ARCHIVE: &str = "bastion-termux-aarch64.tar.gz";
const CHECKSUM: &str = "bastion-termux-aarch64.tar.gz.sha256";
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
}

pub(crate) fn command(state_dir: &Path, command: UpdateCommand) -> Result<()> {
    match command {
        UpdateCommand::Check { force, quiet } => {
            let cache = check(state_dir, force)?;
            if !quiet {
                print_status(&cache);
            }
            Ok(())
        }
        UpdateCommand::Install { yes } => install(state_dir, yes),
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
            let checksum_url = release
                .assets
                .iter()
                .find(|asset| asset.name == CHECKSUM)?
                .browser_download_url
                .clone();
            Some((
                parsed,
                Release {
                    version,
                    tag: release.tag_name,
                    archive_url,
                    checksum_url,
                },
            ))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, release)| release)
        .context("no compatible Bastion release was found")
}

fn install(state_dir: &Path, yes: bool) -> Result<()> {
    ensure_installed_layout()?;
    let cache = check(state_dir, true)?;
    let release = cache
        .release
        .filter(|release| is_newer(&release.version))
        .context("Bastion is already up to date")?;
    if !yes && !confirm(&release.version)? {
        println!("Update cancelled.");
        return Ok(());
    }

    fs::create_dir_all(state_dir)?;
    let staging = state_dir.join(format!("update-staging-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).context("remove stale update staging directory")?;
    }
    fs::create_dir_all(staging.join("payload"))?;
    let result = install_from_release(state_dir, &staging, &release);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn install_from_release(state_dir: &Path, staging: &Path, release: &Release) -> Result<()> {
    let archive = staging.join(ARCHIVE);
    let checksum = staging.join(CHECKSUM);
    download(&release.archive_url, &archive, "release archive")?;
    download(&release.checksum_url, &checksum, "release checksum")?;
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
        if let Some(workspace) = daemon_workspace.as_ref() {
            let _ = start_daemon(&state_dir.to_path_buf(), workspace);
        }
        if let Some(rollback_error) = rollback_error {
            return Err(error).context(format!("rollback also failed: {rollback_error:#}"));
        }
        return Err(error).context("update activation failed; previous binaries were restored");
    }

    if let Some(workspace) = daemon_workspace.as_ref() {
        if let Err(error) = start_daemon(&state_dir.to_path_buf(), workspace) {
            restore(&backup, &destination)
                .context("new daemon failed and previous binaries could not be restored")?;
            let _ = start_daemon(&state_dir.to_path_buf(), workspace);
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

fn download(url: &str, destination: &Path, label: &str) -> Result<()> {
    println!("Downloading {label}…");
    let status = Command::new("curl")
        .args(["-fL", "--retry", "3", "--connect-timeout", "10", "-o"])
        .arg(destination)
        .arg(url)
        .status()
        .with_context(|| format!("download {label}"))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{label} download failed: {status}")
    }
}

fn verify_checksum(archive: &Path, checksum: &Path) -> Result<()> {
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
    let output = Command::new("sha256sum")
        .arg(archive)
        .output()
        .context("calculate release checksum")?;
    if !output.status.success() {
        anyhow::bail!("sha256sum failed: {}", output.status);
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if actual != expected {
        anyhow::bail!("release checksum verification failed");
    }
    println!("Release checksum verified.");
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

    #[test]
    fn compares_stable_and_prerelease_versions() {
        assert!(is_newer("0.1.0-alpha.3"));
        assert!(is_newer("0.1.0"));
        assert!(!is_newer("0.1.0-alpha.2"));
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
}
