//! Verified foreground handoff for the scripts-free npm launcher on older Node.
use anyhow::Result;
#[cfg(unix)]
pub struct Foreground {
    original: Option<libc::pid_t>,
}
#[cfg(not(unix))]
pub struct Foreground;
#[cfg(unix)]
impl Foreground {
    pub fn enter() -> Result<Self> {
        let Some(pid) = std::env::var_os("CEDEGRID_LAUNCHER_PID") else {
            return Ok(Self { original: None });
        };
        let parent: libc::pid_t = pid
            .to_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("invalid launcher identity"))?;
        // Environment alone is never sufficient to move another process's terminal.
        unsafe {
            anyhow::ensure!(
                parent > 1 && parent == libc::getppid(),
                "launcher parent identity mismatch"
            );
            // Keep the launcher's session so this process can own its terminal,
            // but isolate signals from the launcher even with redirected input.
            if libc::getpgrp() != libc::getpid() {
                anyhow::ensure!(
                    libc::setpgid(0, 0) == 0,
                    "cannot create native launcher process group: {}",
                    std::io::Error::last_os_error()
                );
            }
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return Ok(Self { original: None });
            }
            let original = libc::tcgetpgrp(libc::STDIN_FILENO);
            let parent_group = libc::getpgid(parent);
            let own_group = libc::getpgrp();
            if original == own_group {
                return Ok(Self { original: None });
            }
            anyhow::ensure!(
                original > 0 && original == parent_group && own_group == libc::getpid(),
                "launcher does not own the foreground terminal"
            );
            set_foreground(own_group)?;
            Ok(Self {
                original: Some(original),
            })
        }
    }
}
#[cfg(unix)]
unsafe fn set_foreground(group: libc::pid_t) -> Result<()> {
    // tcsetpgrp from the child's initial background group would otherwise stop it.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
    }
    anyhow::ensure!(
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous) } == 0,
        "cannot block terminal handoff signal"
    );
    let result = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, group) };
    let error = std::io::Error::last_os_error();
    let restored =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
    anyhow::ensure!(restored == 0, "cannot restore terminal signal mask");
    if result == -1 {
        return Err(error.into());
    }
    Ok(())
}
#[cfg(unix)]
impl Drop for Foreground {
    fn drop(&mut self) {
        if let Some(original) = self.original {
            unsafe {
                let _ = set_foreground(original);
            }
        }
    }
}
#[cfg(not(unix))]
impl Foreground {
    pub fn enter() -> Result<Self> {
        Ok(Self)
    }
}
