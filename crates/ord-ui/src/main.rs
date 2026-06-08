//! `ord-ui` — the clip library window.

use std::path::PathBuf;

fn clips_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("Videos/open-recorder")
}

#[cfg(feature = "gui")]
fn main() {
    // Record panics + UI stalls to the diagnostics log so crashes/ANRs are
    // captured by the program itself.
    ord_ui::diag::install_panic_hook();
    if let Err(e) = ord_ui::app::run(clips_dir()) {
        eprintln!("ord-ui: {e}");
        ord_ui::diag::log_line(&format!("ord-ui exited with error: {e}"));
        std::process::exit(1);
    }
}

#[cfg(not(feature = "gui"))]
fn main() {
    // Without the gui feature there is no window; list clips to stdout so the
    // binary is still useful for a quick check.
    let dir = clips_dir();
    let clips = ord_ui::scan_dir(&dir);
    if clips.is_empty() {
        println!("no clips in {}", dir.display());
    } else {
        for clip in clips {
            println!("{}\t{}", clip.label(), clip.path.display());
        }
    }
}
