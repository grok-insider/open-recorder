//! In-program diagnostics (gui-only): a panic hook and a UI watchdog that write
//! to `/tmp/ord-ui-debug.log`, so hangs (the "Application Not Responding"
//! dialog) and crashes are captured by the program itself with context.
//!
//! - The **panic hook** appends the panic message, location, and a backtrace.
//! - The **watchdog** runs on its own thread; the UI calls [`Watchdog::beat`]
//!   each frame with a short stage label. If no beat arrives for a while, it logs
//!   `UI STALL` with the elapsed time and the last stage — localizing where the
//!   main thread is stuck even though we can't unwind the hung thread remotely.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The shared diagnostics log path (`$ORD_DEBUG_LOG` or `/tmp/ord-ui-debug.log`).
pub fn log_path() -> std::path::PathBuf {
    std::env::var("ORD_DEBUG_LOG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/ord-ui-debug.log"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Append a timestamped line to the diagnostics log (best-effort).
pub fn log_line(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "{} {msg}", now_ms());
    }
}

/// Install a panic hook that records panics (with a backtrace) to the log, then
/// chains to the previous hook. Call once at startup.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let bt = std::backtrace::Backtrace::force_capture();
        log_line(&format!("PANIC thread='{name}': {info}\n{bt}"));
        prev(info);
    }));
}

/// Watchdog for the UI thread: detects stalls and logs them.
#[derive(Clone)]
pub struct Watchdog {
    last_beat: Arc<AtomicU64>,
    stage: Arc<Mutex<&'static str>>,
    stalled: Arc<AtomicBool>,
}

impl Watchdog {
    /// Start the watchdog thread. A gap larger than `threshold` between beats is
    /// reported as a stall (logged once per stall, with recovery noted).
    pub fn start(threshold: Duration) -> Self {
        let wd = Watchdog {
            last_beat: Arc::new(AtomicU64::new(now_ms())),
            stage: Arc::new(Mutex::new("init")),
            stalled: Arc::new(AtomicBool::new(false)),
        };
        let mon = wd.clone();
        let threshold_ms = threshold.as_millis() as u64;
        std::thread::Builder::new()
            .name("ord-watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(500));
                let gap = now_ms().saturating_sub(mon.last_beat.load(Ordering::Relaxed));
                if gap > threshold_ms {
                    if !mon.stalled.swap(true, Ordering::Relaxed) {
                        let stage = *mon.stage.lock().unwrap();
                        log_line(&format!(
                            "UI STALL: {gap}ms with no frame, last stage='{stage}'"
                        ));
                    }
                } else if mon.stalled.swap(false, Ordering::Relaxed) {
                    log_line("UI recovered from stall");
                }
            })
            .ok();
        wd
    }

    /// Record liveness at a named stage. Call frequently from the UI thread.
    pub fn beat(&self, stage: &'static str) {
        self.last_beat.store(now_ms(), Ordering::Relaxed);
        *self.stage.lock().unwrap() = stage;
    }
}
