//! Clip storage policy: filename templates and library pruning.
//!
//! Both halves are pure (template rendering and prune *planning* take values
//! and return values); the daemon's writer applies the plan with trivial I/O.

use std::path::{Path, PathBuf};

/// What kind of file is being named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    /// A replay-buffer save.
    Clip,
    /// A full-length manual recording.
    Recording,
}

/// Render a clip filename (no extension) from the storage template.
///
/// Tokens: `{game}` (slug or "clip"), `{rec}` (`""`/`"-rec"`), `{epoch}`
/// (unix seconds), `{date}` (`YYYY-MM-DD`), `{time}` (`HHMMSS`). The result
/// may contain `/` (subfolders, e.g. date folders); leading slashes are
/// stripped so a template can never escape the clips directory, and any `..`
/// segment is dropped for the same reason.
pub fn render_name(template: &str, game: Option<&str>, kind: ClipKind, epoch: u64) -> PathBuf {
    let (date, time) = date_time_parts(epoch);
    let rendered = template
        .replace("{game}", game.unwrap_or("clip"))
        .replace(
            "{rec}",
            match kind {
                ClipKind::Clip => "",
                ClipKind::Recording => "-rec",
            },
        )
        .replace("{epoch}", &epoch.to_string())
        .replace("{date}", &date)
        .replace("{time}", &time);

    let mut out = PathBuf::new();
    for part in rendered.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(format!("clip-{epoch}"));
    }
    out
}

/// Civil date/time for a unix epoch (UTC): (`YYYY-MM-DD`, `HHMMSS`).
/// Days-from-epoch conversion per Howard Hinnant's algorithm.
fn date_time_parts(epoch: u64) -> (String, String) {
    let days = (epoch / 86_400) as i64;
    let secs = epoch % 86_400;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    (
        format!("{y:04}-{mo:02}-{d:02}"),
        format!("{h:02}{m:02}{s:02}"),
    )
}

/// One candidate file for pruning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneEntry {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Modification time as unix seconds.
    pub mtime_epoch: u64,
}

/// Decide which files to delete so the library obeys the storage policy:
/// files older than `max_age_days` go first, then the oldest files until the
/// total size fits under `max_gib`. Returns paths to delete, oldest first.
/// Pure: the caller lists files and performs the deletions.
pub fn plan_prune(
    mut entries: Vec<PruneEntry>,
    max_gib: Option<u32>,
    max_age_days: Option<u32>,
    now_epoch: u64,
) -> Vec<PathBuf> {
    entries.sort_by_key(|e| e.mtime_epoch);
    let mut doomed: Vec<PathBuf> = Vec::new();
    let mut kept: Vec<&PruneEntry> = Vec::new();

    for e in &entries {
        let too_old = max_age_days
            .map(|days| e.mtime_epoch.saturating_add(days as u64 * 86_400) < now_epoch)
            .unwrap_or(false);
        if too_old {
            doomed.push(e.path.clone());
        } else {
            kept.push(e);
        }
    }

    if let Some(gib) = max_gib {
        let budget = gib as u64 * 1024 * 1024 * 1024;
        let mut total: u64 = kept.iter().map(|e| e.size_bytes).sum();
        for e in &kept {
            if total <= budget {
                break;
            }
            total = total.saturating_sub(e.size_bytes);
            doomed.push(e.path.clone());
        }
    }
    doomed
}

/// List prune candidates in `dir`: regular video files only, top level only —
/// the `exports/` subdirectory (and any other folder) is never touched.
pub fn prune_candidates(dir: &Path) -> Vec<PruneEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            if ext != "mkv" && ext != "mp4" {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime_epoch = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some(PruneEntry {
                path,
                size_bytes: meta.len(),
                mtime_epoch,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_renders_all_tokens() {
        // 2026-06-10 07:30:00 UTC == 1781076600.
        let name = render_name(
            "{date}/{game}{rec}-{epoch}-{time}",
            Some("hades"),
            ClipKind::Recording,
            1_781_076_600,
        );
        assert_eq!(
            name,
            PathBuf::from("2026-06-10/hades-rec-1781076600-073000")
        );
    }

    #[test]
    fn default_template_matches_legacy_names() {
        let clip = render_name("{game}{rec}-{epoch}", Some("doom"), ClipKind::Clip, 42);
        assert_eq!(clip, PathBuf::from("doom-42"));
        let rec = render_name("{game}{rec}-{epoch}", None, ClipKind::Recording, 42);
        assert_eq!(rec, PathBuf::from("clip-rec-42"));
    }

    #[test]
    fn template_cannot_escape_the_clips_dir() {
        let name = render_name("../../{game}/{epoch}", Some("x"), ClipKind::Clip, 7);
        assert_eq!(name, PathBuf::from("x/7"));
        let abs = render_name("/etc/{game}", Some("x"), ClipKind::Clip, 7);
        assert_eq!(abs, PathBuf::from("etc/x"));
        // A template that renders to nothing still yields a usable name.
        assert_eq!(
            render_name("", None, ClipKind::Clip, 7),
            PathBuf::from("clip-7")
        );
    }

    fn entry(name: &str, gib: u64, mtime: u64) -> PruneEntry {
        PruneEntry {
            path: PathBuf::from(name),
            size_bytes: gib * 1024 * 1024 * 1024,
            mtime_epoch: mtime,
        }
    }

    #[test]
    fn prune_by_age_only() {
        let now = 100 * 86_400;
        let files = vec![
            entry("old.mkv", 1, 10 * 86_400), // 90 days old
            entry("new.mkv", 1, 99 * 86_400), // 1 day old
        ];
        let doomed = plan_prune(files, None, Some(30), now);
        assert_eq!(doomed, vec![PathBuf::from("old.mkv")]);
    }

    #[test]
    fn prune_by_size_drops_oldest_first() {
        let files = vec![
            entry("c.mkv", 4, 30),
            entry("a.mkv", 4, 10),
            entry("b.mkv", 4, 20),
        ];
        // 12 GiB held, 8 GiB budget -> drop the single oldest (a), 8 <= 8.
        let doomed = plan_prune(files, Some(8), None, 100);
        assert_eq!(doomed, vec![PathBuf::from("a.mkv")]);
    }

    #[test]
    fn prune_age_then_size_combined() {
        let now = 100 * 86_400;
        let files = vec![
            entry("ancient.mkv", 1, 1), // age-doomed
            entry("big1.mkv", 6, 90 * 86_400),
            entry("big2.mkv", 6, 95 * 86_400),
        ];
        // After the age pass, 12 GiB remain vs a 6 GiB budget -> drop big1 too.
        let doomed = plan_prune(files, Some(6), Some(30), now);
        assert_eq!(
            doomed,
            vec![PathBuf::from("ancient.mkv"), PathBuf::from("big1.mkv")]
        );
    }

    #[test]
    fn no_policy_prunes_nothing() {
        let files = vec![entry("a.mkv", 100, 1)];
        assert!(plan_prune(files, None, None, u64::MAX).is_empty());
    }

    #[test]
    fn candidates_skip_subdirs_and_non_video() {
        let dir = std::env::temp_dir().join(format!("ord-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("exports")).unwrap();
        std::fs::write(dir.join("a.mkv"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join("exports/keep.mp4"), b"x").unwrap();
        let found = prune_candidates(&dir);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("a.mkv"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
