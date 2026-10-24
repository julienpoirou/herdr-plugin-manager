//! Running a plugin action from the popup.
//!
//! Herdr refuses anything that opens UI while a popup is showing
//! (`ui_busy`), so the popup spawns a detached copy of itself, exits (which
//! closes the popup), and the copy invokes the action once the popup's
//! process is gone. The copy runs in its own session so closing the popup's
//! process group does not take it down.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::herdr::Herdr;

pub fn spawn_invoke_after_exit(action: &str) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find own executable")?;
    let mut command = Command::new(exe);
    command
        .arg("__invoke-after-exit")
        .arg(std::process::id().to_string())
        .arg(action)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    // spawn() returns after exec on unix (setsid already happened), so the
    // child is detached before we exit — no race with the popup teardown.
    command
        .spawn()
        .context("could not start the action helper")?;
    Ok(())
}

fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

pub fn invoke_after_exit(herdr: &Herdr, pid: u32, action: &str) {
    let start = Instant::now();
    while alive(pid) && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(not(unix))]
    std::thread::sleep(Duration::from_millis(400));
    // Herdr needs a moment to return to the workspace view.
    for _ in 0..15 {
        match herdr.invoke(action) {
            Ok(()) => return,
            Err(err) => {
                let busy = err
                    .downcast_ref::<crate::herdr::HerdrError>()
                    .is_some_and(|e| e.code.as_deref() == Some("ui_busy"));
                if busy || err.to_string().contains("popup") {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                return;
            }
        }
    }
}
