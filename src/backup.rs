//! Opt-in, local-only snapshot worker. File locks establish ownership; a
//! generation file requests shutdown without trusting or signalling stored PIDs.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::herdr::Herdr;
use crate::{gitops, paths};

const POLL: Duration = Duration::from_secs(2);
const SHUTDOWN: Duration = Duration::from_secs(15);
const GENERATION_ENV: &str = "HERDR_PM_BACKUP_GENERATION";

#[derive(Serialize, Deserialize)]
struct Generation {
    token: String,
    executable: PathBuf,
    modified: u128,
    length: u64,
}

pub fn error_log() -> PathBuf {
    paths::pm_home().join("backup-watch.log")
}

fn open_lock(name: &str) -> Result<File> {
    let path = paths::pm_home().join(name);
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))
}

fn try_own(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err.into()),
    }
}

fn control_path() -> PathBuf {
    paths::pm_home().join("backup-watch.json")
}

fn generation() -> Result<Option<Generation>> {
    match std::fs::read(control_path()) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid backup worker control file")?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn clear_generation() -> Result<()> {
    match std::fs::remove_file(control_path()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn executable(herdr: &Herdr) -> Result<PathBuf> {
    let manager = herdr
        .list()?
        .into_iter()
        .find(|plugin| plugin.plugin_id == paths::PLUGIN_ID);
    let path = match manager {
        Some(plugin) => PathBuf::from(plugin.plugin_root)
            .join("bin")
            .join(if cfg!(windows) {
                "herdr-pm.exe"
            } else {
                "herdr-pm"
            }),
        // Standalone CLI use does not require installing the manager as a plugin.
        None => std::env::current_exe()?,
    };
    path.canonicalize()
        .with_context(|| format!("could not locate backup worker {}", path.display()))
}

fn identity(path: &Path) -> Result<(u128, u64)> {
    let metadata = path.metadata()?;
    let modified = metadata
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    Ok((modified, metadata.len()))
}

fn wait_stopped(lock: &File) -> Result<()> {
    let until = Instant::now() + SHUTDOWN;
    loop {
        if try_own(lock)? {
            return Ok(());
        }
        if Instant::now() >= until {
            bail!("backup worker has not stopped yet (it may be waiting for an active GitOps operation); retry after it finishes. Errors: {}", error_log().display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts, stops, or replaces the worker after a local mutation. Captures
/// immediately when enabled; never commits, contacts a remote, or kills a PID.
pub fn refresh(herdr: &Herdr, config: &Config) -> Result<()> {
    let result = refresh_inner(herdr, config);
    if let Err(err) = &result {
        let _ = record_error(&format!("backup refresh failed: {err:#}"));
    }
    result
}

fn refresh_inner(herdr: &Herdr, config: &Config) -> Result<()> {
    if herdr.dry_run {
        return Ok(());
    }
    if !config.backup.enabled && !paths::pm_home().exists() {
        return Ok(());
    }
    std::fs::create_dir_all(paths::pm_home())?;
    let supervisor = open_lock(".backup-supervisor.lock")?;
    supervisor.lock()?;
    let worker = open_lock(".backup-worker.lock")?;
    let idle = try_own(&worker)?;
    if !config.backup.enabled {
        clear_generation()?;
        if !idle {
            wait_stopped(&worker)?;
        }
        return Ok(());
    }
    config.check()?;
    let target = executable(herdr)?;
    let (modified, length) = identity(&target)?;
    let previous = generation()?;
    let current = previous.as_ref().is_some_and(|state| {
        state.executable == target && state.modified == modified && state.length == length
    });
    if !idle && current {
        // Already owned by a live worker. Also snapshot changes made by this
        // command now, rather than waiting for the next polling interval.
        return gitops::capture(herdr, config);
    }
    clear_generation()?;
    if !idle {
        wait_stopped(&worker)?;
    }
    // Even a failed first snapshot gets a live worker, so correcting an
    // external file can recover without toggling the preference again.
    let captured = gitops::capture(herdr, config);
    let state = Generation {
        token: format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_nanos()
        ),
        executable: target.clone(),
        modified,
        length,
    };
    let temporary = paths::pm_home().join("backup-watch.next");
    std::fs::write(&temporary, serde_json::to_vec(&state)?)?;
    std::fs::rename(&temporary, control_path())?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(error_log())?;
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new(&target);
        command.arg("backup-watch");
        command
    };
    #[cfg(windows)]
    let mut command =
        {
            // Stable Rust inherits all inheritable Windows handles. ShellExecute
            // launches with no inherited handles, so a detached worker cannot keep
            // a caller's captured stdout/stderr pipes open.
            let mut command = Command::new("powershell.exe");
            command.args([
            "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; $s=New-Object System.Diagnostics.ProcessStartInfo; \
             $s.FileName=$env:HERDR_PM_BACKUP_EXECUTABLE; $s.Arguments='backup-watch'; \
             $s.WorkingDirectory=$env:HERDR_PM_HOME; $s.UseShellExecute=$true; \
             $s.WindowStyle=[System.Diagnostics.ProcessWindowStyle]::Hidden; \
             $p=[System.Diagnostics.Process]::Start($s); $p.Dispose()",
        ]);
            command.env("HERDR_PM_BACKUP_EXECUTABLE", &target);
            command
        };
    command
        .env(GENERATION_ENV, &state.token)
        .env("HERDR_PM_HOME", paths::pm_home())
        .env("HERDR_PM_CONFIG_DIR", paths::pm_config_dir())
        .env(
            "HERDR_PM_HERDR_BIN",
            if herdr.bin.is_file() {
                herdr.bin.canonicalize()?
            } else {
                herdr.bin.clone()
            },
        )
        .current_dir(paths::pm_home())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // A detached worker has no plugin socket capability. Retain ordinary user
    // configuration/credential environment, not authorization from a pane.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if name.starts_with("HERDR_PLUGIN_")
            || (name.starts_with("HERDR_")
                && (name.contains("SOCKET") || name.contains("AUTH") || name.contains("TOKEN")))
        {
            command.env_remove(key);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW for the short-lived launcher
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // setsid is async-signal-safe; no allocation or locks after fork.
        unsafe {
            command.pre_exec(|| {
                unsafe extern "C" {
                    fn setsid() -> std::os::raw::c_int;
                }
                if setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    // Release ownership before spawning; the supervisor prevents competing
    // refresh calls, while only the matching generation may acquire the lock.
    drop(worker);
    #[cfg(windows)]
    let spawned = command.status().and_then(|status| {
        if status.success() {
            Ok(None)
        } else {
            Err(std::io::Error::other(format!(
                "backup launcher exited with {status}"
            )))
        }
    });
    #[cfg(not(windows))]
    let spawned = command.spawn().map(Some);
    let mut child: Option<std::process::Child> = match spawned {
        Ok(child) => child,
        Err(err) => {
            clear_generation()?;
            return Err(err)
                .with_context(|| format!("could not start backup worker {}", target.display()));
        }
    };
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(child) = child.as_mut() {
            if let Some(status) = child.try_wait()? {
                clear_generation()?;
                bail!(
                    "backup worker exited with {status}; see {}",
                    error_log().display()
                );
            }
        }
        let lock = open_lock(".backup-worker.lock")?;
        let owned_by_worker = !try_own(&lock)?;
        drop(lock);
        if owned_by_worker {
            // Reap our child when this parent remains alive (notably the TUI).
            if let Some(mut child) = child {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            return captured;
        }
        if Instant::now() >= until {
            clear_generation()?;
            if let Some(mut child) = child {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            bail!(
                "backup worker did not acknowledge startup; shutdown requested. See {}",
                error_log().display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn record_error(message: &str) -> Result<()> {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(error_log())?;
    writeln!(
        log,
        "{} {message}",
        crate::util::format_iso8601(crate::util::now_unix())
    )?;
    Ok(())
}

enum WatchStep {
    Stop,
    Captured,
    Replace,
}

fn watch_step(herdr: &Herdr, token: &str) -> Result<WatchStep> {
    let Some(state) = generation()? else {
        return Ok(WatchStep::Stop);
    };
    if state.token != token {
        return Ok(WatchStep::Stop);
    }
    let config = Config::load_read_only()?;
    if !config.backup.enabled {
        return Ok(WatchStep::Stop);
    }
    let target = executable(herdr)?;
    let (modified, length) = identity(&target)?;
    if state.executable != target || state.modified != modified || state.length != length {
        return Ok(WatchStep::Replace);
    }
    gitops::capture(herdr, &config)?;
    Ok(WatchStep::Captured)
}

/// Hidden worker entry. Direct invocation without refresh's generation cannot
/// create an untracked worker. The OS lock is the liveness/ownership proof.
pub fn watch() -> Result<()> {
    let result = watch_inner();
    if let Err(err) = &result {
        let _ = record_error(&format!("backup worker stopped: {err:#}"));
    }
    result
}

fn watch_inner() -> Result<()> {
    let token =
        std::env::var(GENERATION_ENV).context("backup-watch must be started by backup::refresh")?;
    let worker = open_lock(".backup-worker.lock")?;
    // The parent's startup probe briefly acquires this same lock. Do not
    // mistake that probe for a competing long-lived worker.
    wait_stopped(&worker)?;
    let herdr = Herdr::detect(false);
    let mut last_error = String::new();
    loop {
        match watch_step(&herdr, &token) {
            Ok(WatchStep::Stop) => return Ok(()),
            Ok(WatchStep::Replace) => {
                drop(worker);
                // Reload after releasing ownership: preferences may have
                // changed while an update switched the executable.
                return refresh(&herdr, &Config::load_read_only()?);
            }
            Ok(WatchStep::Captured) => {
                if !last_error.is_empty() {
                    record_error("backup capture recovered")?;
                    last_error.clear();
                }
            }
            Err(err) => {
                let message = format!("backup iteration failed (will retry): {err:#}");
                if message != last_error {
                    record_error(&message)?;
                    last_error = message;
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetime_lock_proves_liveness_and_releases_without_pid_signals() {
        let directory = std::env::temp_dir().join(format!(
            "hpm-backup-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("worker.lock");
        let first = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let next = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(try_own(&first).unwrap());
        assert!(!try_own(&next).unwrap());
        drop(first);
        assert!(try_own(&next).unwrap());
        drop(next);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
