//! The post-save hook: an optional user program run after every saved clip.
//!
//! Mirrors gpu-screen-recorder's `-sc` script: the program receives the clip
//! path as its first argument and runs **asynchronously, off the capture
//! path** — a slow or hung hook can never stall the daemon, and its exit status
//! is only logged.

use std::path::{Path, PathBuf};

/// Expand a leading `~/` to `$HOME` so config values like
/// `on_clip_saved = "~/bin/clip-hook"` work as users expect. Anything else is
/// returned untouched.
pub fn expand_home(program: &str) -> PathBuf {
    match (
        program.strip_prefix("~/"),
        std::env::var("HOME").ok().filter(|h| !h.is_empty()),
    ) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(program),
    }
}

/// Spawn `program <clip>` detached from the save path. Returns the reaper
/// thread handle (production ignores it; tests join it for determinism).
///
/// Failure to spawn is logged, never propagated: the clip is already safely on
/// disk and a broken hook must not turn a successful save into an error.
pub fn spawn_clip_hook(program: &str, clip: &Path) -> Option<std::thread::JoinHandle<()>> {
    let program = expand_home(program);
    match std::process::Command::new(&program).arg(clip).spawn() {
        Ok(mut child) => {
            let program = program.display().to_string();
            // Reap the child so finished hooks don't accumulate as zombies.
            Some(std::thread::spawn(move || match child.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    tracing::warn!(hook = %program, %status, "clip hook exited non-zero")
                }
                Err(e) => tracing::warn!(hook = %program, error = %e, "clip hook wait failed"),
            }))
        }
        Err(e) => {
            tracing::warn!(
                hook = %program.display(),
                error = %e,
                "could not run on_clip_saved hook; check the path in config.toml"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn expands_leading_tilde_only() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/bin/hook"), Path::new(&home).join("bin/hook"));
        assert_eq!(expand_home("/usr/bin/hook"), PathBuf::from("/usr/bin/hook"));
        assert_eq!(expand_home("hook"), PathBuf::from("hook"));
    }

    #[test]
    fn hook_receives_clip_path() {
        let dir = std::env::temp_dir().join(format!("ord-hook-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        let script = dir.join("hook.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$1\" > {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let clip = dir.join("game-123.mkv");
        let handle = spawn_clip_hook(&script.to_string_lossy(), &clip).expect("script spawns");
        handle.join().unwrap();

        let recorded = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(recorded.trim(), clip.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_hook_program_is_logged_not_fatal() {
        assert!(spawn_clip_hook("/nonexistent/ord-hook", Path::new("/tmp/x.mkv")).is_none());
    }
}
