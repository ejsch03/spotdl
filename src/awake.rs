//! Keeping the computer awake while downloads run.
//!
//! A download can take many hours (one audio key every `--delay` seconds), and a computer
//! that idles into sleep partway through stops it until someone wakes it. This holds off
//! idle sleep on macOS and Windows. On macOS it also keeps the app out of App Nap, which
//! throttles the timers and network of apps whose window is hidden, minimized or covered.
//! Elsewhere it does nothing.

/// Keeps the computer from idling into sleep for as long as it lives. The display can
/// still turn off, and closing a laptop's lid still puts it to sleep.
pub struct KeepAwake {
    /// Dropping it ends the activity.
    _release: std::sync::mpsc::Sender<()>,
}

impl KeepAwake {
    /// `reason` is what macOS shows for it, e.g. in `pmset -g assertions`.
    pub fn new(reason: &str) -> Self {
        let (release, released) = std::sync::mpsc::channel::<()>();
        // a thread of its own holds it: macOS's activity lasts while a block runs, and
        // Windows' setting belongs to the thread that made it
        #[cfg(any(target_os = "macos", windows))]
        {
            let reason = reason.to_string();
            let spawned = std::thread::Builder::new()
                .name("keep-awake".into())
                .spawn(move || {
                    #[cfg(target_os = "macos")]
                    macos::hold(&reason, released);
                    #[cfg(windows)]
                    windows::hold(&reason, released);
                });
            if let Err(e) = spawned {
                log::warn!("couldn't keep the computer awake while downloading: {e}");
            }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = (reason, released);
        Self { _release: release }
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    /// Keeps Windows from idling into sleep until the sender of `released` is dropped.
    /// The setting is this thread's, so it also ends if the thread does.
    pub fn hold(_reason: &str, released: std::sync::mpsc::Receiver<()>) {
        // SAFETY: SetThreadExecutionState takes plain flags and touches nothing of ours.
        if unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) } == 0 {
            log::warn!("Windows didn't let spotdl keep the computer awake");
        }
        _ = released.recv();
        // SAFETY: as above
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    /// Holds a user-initiated activity (no idle sleep, no App Nap) until the sender of
    /// `released` is dropped. macOS ends the activity when the block returns, so this
    /// needs no cleanup of its own, and none if the process exits.
    pub fn hold(reason: &str, released: std::sync::mpsc::Receiver<()>) {
        let block = block2::RcBlock::new(move || _ = released.recv());
        NSProcessInfo::processInfo().performActivityWithOptions_reason_usingBlock(
            NSActivityOptions::UserInitiated,
            &NSString::from_str(reason),
            &block,
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Whether `pmset` lists an idle sleep assertion held by this process.
    fn holding() -> bool {
        let out = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset runs");
        let me = format!("pid {}(", std::process::id());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line.contains(&me) && line.contains("PreventUserIdleSystemSleep"))
    }

    fn eventually(want: bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if holding() == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn keeps_the_mac_awake_until_dropped() {
        let awake = KeepAwake::new("spotdl test");
        assert!(eventually(true), "no idle sleep assertion while held");
        drop(awake);
        assert!(eventually(false), "the assertion outlived the guard");
    }
}
