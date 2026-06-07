//! Clip library model: discover saved clips on disk and present them sorted,
//! newest first. Pure logic (no GUI), fully tested. The egui view renders from
//! this.

use std::path::{Path, PathBuf};

/// One discovered clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub path: PathBuf,
    /// File stem (e.g. "path-of-exile-1780000000").
    pub stem: String,
    /// Epoch seconds parsed from the trailing `-<digits>` of the stem, if any.
    pub epoch: Option<u64>,
}

impl Clip {
    /// The human label: the game part of the stem (stem minus the trailing
    /// `-<epoch>`), or the whole stem if there is no epoch suffix.
    pub fn label(&self) -> &str {
        match self.epoch {
            Some(_) => self
                .stem
                .rsplit_once('-')
                .map(|(head, _)| head)
                .unwrap_or(&self.stem),
            None => &self.stem,
        }
    }
}

/// Parse a clip from a path if it is an `.mkv`/`.mp4` file. Extracts the trailing
/// `-<digits>` epoch when present.
pub fn parse_clip(path: &Path) -> Option<Clip> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext != "mkv" && ext != "mp4" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.to_string();
    let epoch = stem
        .rsplit_once('-')
        .and_then(|(_, tail)| tail.parse::<u64>().ok());
    Some(Clip {
        path: path.to_path_buf(),
        stem,
        epoch,
    })
}

/// Sort clips newest-first: clips with an epoch come before those without; among
/// those with an epoch, higher (newer) first; ties broken by stem for stability.
pub fn sort_newest_first(clips: &mut [Clip]) {
    // Sort key: (has_no_epoch, descending-epoch, stem). `false` < `true`, so
    // clips that HAVE an epoch (has_no_epoch = false) sort first.
    clips.sort_by(|a, b| {
        let a_key = (a.epoch.is_none(), std::cmp::Reverse(a.epoch.unwrap_or(0)));
        let b_key = (b.epoch.is_none(), std::cmp::Reverse(b.epoch.unwrap_or(0)));
        a_key.cmp(&b_key).then_with(|| a.stem.cmp(&b.stem))
    });
}

/// Scan a directory for clips, sorted newest-first. Missing dir -> empty list.
pub fn scan_dir(dir: &Path) -> Vec<Clip> {
    let mut clips: Vec<Clip> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| parse_clip(&e.path()))
            .collect(),
        Err(_) => Vec::new(),
    };
    sort_newest_first(&mut clips);
    clips
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(stem: &str) -> Clip {
        parse_clip(Path::new(&format!("/clips/{stem}.mkv"))).unwrap()
    }

    #[test]
    fn parses_mkv_with_epoch() {
        let c = clip("path-of-exile-1780000000");
        assert_eq!(c.epoch, Some(1780000000));
        assert_eq!(c.label(), "path-of-exile");
    }

    #[test]
    fn parses_without_epoch() {
        let c = clip("manual-clip");
        // "clip" is digits? no -> epoch None, label is whole stem.
        assert_eq!(c.epoch, None);
        assert_eq!(c.label(), "manual-clip");
    }

    #[test]
    fn rejects_non_video() {
        assert!(parse_clip(Path::new("/clips/notes.txt")).is_none());
        assert!(parse_clip(Path::new("/clips/thumb.png")).is_none());
    }

    #[test]
    fn accepts_mp4_too() {
        assert!(parse_clip(Path::new("/clips/x-123.mp4")).is_some());
    }

    #[test]
    fn sorts_newest_first() {
        let mut v = vec![
            clip("a-100"),
            clip("b-300"),
            clip("c-200"),
            clip("no-epoch-here"),
        ];
        sort_newest_first(&mut v);
        assert_eq!(v[0].epoch, Some(300));
        assert_eq!(v[1].epoch, Some(200));
        assert_eq!(v[2].epoch, Some(100));
        // no-epoch sorts last.
        assert_eq!(v[3].epoch, None);
    }

    #[test]
    fn scan_missing_dir_is_empty() {
        let clips = scan_dir(Path::new("/nonexistent/open-recorder/clips"));
        assert!(clips.is_empty());
    }

    #[test]
    fn scan_real_dir() {
        let dir = std::env::temp_dir().join(format!("ord-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("game-200.mkv"), b"x").unwrap();
        std::fs::write(dir.join("game-100.mkv"), b"x").unwrap();
        std::fs::write(dir.join("readme.txt"), b"x").unwrap();
        let clips = scan_dir(&dir);
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].epoch, Some(200));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
