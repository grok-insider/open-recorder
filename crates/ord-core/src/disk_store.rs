//! Disk-backed replay store (gpu-screen-recorder's `-replay-storage disk`).
//!
//! Implements [`FrameStore`](crate::store::FrameStore) by keeping only per-frame
//! metadata (pts/dts/keyframe + a file offset) in RAM and spilling the encoded
//! payloads to a single spill file. This lets the replay window be much longer
//! than RAM allows (minutes of 1440p) on low-memory boxes, at the cost of a disk
//! read per saved frame (only the frames actually saved are read back).
//!
//! Semantics mirror [`RingBuffer`](crate::ring::RingBuffer): the metadata index
//! is kept in pts order (correcting the occasional B-frame reorder), eviction is
//! a time window anchored to the newest pts seen, and `window` materializes a
//! range in order. Payloads are appended in arrival order; eviction leaves holes
//! that a periodic compaction reclaims, so disk use stays bounded to roughly the
//! live window. The hot path (`push`) does one positioned write and no
//! allocation beyond the metadata entry.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use bytes::Bytes;

use crate::ring::EncodedFrame;
use crate::store::{FrameMeta, FrameStore};
use crate::Ticks;

/// One frame's location + metadata. The payload lives at `[offset, offset+len)`
/// in the spill file.
#[derive(Debug, Clone, Copy)]
struct Entry {
    offset: u64,
    len: u32,
    pts: Ticks,
    dts: Ticks,
    is_keyframe: bool,
}

/// Compaction triggers once dead (evicted) payload bytes exceed this AND make up
/// more than half the file, so steady-state churn rewrites the file rarely.
const COMPACT_MIN_DEAD_BYTES: u64 = 32 * 1024 * 1024;

/// A bounded, time-windowed store that spills encoded payloads to disk.
pub struct DiskFrameStore {
    path: PathBuf,
    file: File,
    /// Metadata in pts order (oldest first).
    index: VecDeque<Entry>,
    /// Next append position in the spill file.
    write_offset: u64,
    /// Live payload bytes (sum of `index` lens).
    live_bytes: usize,
    /// Evicted payload bytes still occupying file space (reclaimed on compaction).
    dead_bytes: u64,
    capacity_ticks: Ticks,
    ticks_per_sec: i64,
    max_pts: Ticks,
}

impl DiskFrameStore {
    /// Create a fresh spill file at `path` (truncating any existing one) for a
    /// `capacity_seconds` window with frame pts in `ticks_per_sec` units.
    pub fn create(
        path: impl Into<PathBuf>,
        capacity_seconds: u32,
        ticks_per_sec: i64,
    ) -> io::Result<Self> {
        debug_assert!(capacity_seconds >= 1);
        debug_assert!(ticks_per_sec >= 1);
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            path,
            file,
            index: VecDeque::new(),
            write_offset: 0,
            live_bytes: 0,
            dead_bytes: 0,
            capacity_ticks: capacity_seconds.max(1) as i64 * ticks_per_sec,
            ticks_per_sec,
            max_pts: i64::MIN,
        })
    }

    /// Insert `entry` into the pts-ordered index (append-fast-path, else sorted
    /// insert for the occasional reorder), like the RAM ring.
    fn insert_ordered(&mut self, entry: Entry) {
        if self
            .index
            .back()
            .map(|b| entry.pts >= b.pts)
            .unwrap_or(true)
        {
            self.index.push_back(entry);
        } else {
            let pos = self
                .index
                .iter()
                .position(|e| e.pts > entry.pts)
                .unwrap_or(self.index.len());
            self.index.insert(pos, entry);
        }
    }

    fn evict_before(&mut self, cutoff: Ticks) {
        while let Some(front) = self.index.front() {
            if front.pts >= cutoff {
                break;
            }
            if let Some(removed) = self.index.pop_front() {
                self.live_bytes -= removed.len as usize;
                self.dead_bytes += removed.len as u64;
            }
        }
    }

    /// Reclaim file space when evicted payloads dominate the file: copy live
    /// frames (one at a time, bounded memory) into a fresh spill file and swap.
    fn maybe_compact(&mut self) {
        if self.dead_bytes < COMPACT_MIN_DEAD_BYTES || self.dead_bytes * 2 <= self.write_offset {
            return;
        }
        if let Err(e) = self.compact() {
            // Compaction is an optimization; on failure keep serving from the
            // existing (larger) file rather than risk losing the buffer.
            tracing::warn!(error = %e, "replay disk compaction failed; continuing");
        }
    }

    fn compact(&mut self) -> io::Result<()> {
        let tmp_path = self.path.with_extension("compact");
        let tmp = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        let mut new_offset = 0u64;
        let mut buf = Vec::new();
        let mut new_index: VecDeque<Entry> = VecDeque::with_capacity(self.index.len());
        for e in &self.index {
            buf.resize(e.len as usize, 0);
            self.file.read_exact_at(&mut buf, e.offset)?;
            tmp.write_all_at(&buf, new_offset)?;
            new_index.push_back(Entry {
                offset: new_offset,
                ..*e
            });
            new_offset += e.len as u64;
        }
        tmp.sync_data().ok();
        std::fs::rename(&tmp_path, &self.path)?;
        self.file = tmp;
        self.index = new_index;
        self.write_offset = new_offset;
        self.dead_bytes = 0;
        Ok(())
    }
}

impl Drop for DiskFrameStore {
    fn drop(&mut self) {
        // The replay spill is ephemeral; don't leave it behind.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("compact"));
    }
}

impl FrameStore for DiskFrameStore {
    fn push(&mut self, frame: EncodedFrame) {
        let len = frame.data.len();
        // Append the payload at the current end of file (arrival order on disk;
        // the index keeps pts order independently).
        if self
            .file
            .write_all_at(&frame.data, self.write_offset)
            .is_err()
        {
            // A failed spill write must not panic the hot path; drop the frame.
            tracing::warn!("replay disk write failed; dropping frame");
            return;
        }
        let entry = Entry {
            offset: self.write_offset,
            len: len as u32,
            pts: frame.pts,
            dts: frame.dts,
            is_keyframe: frame.is_keyframe,
        };
        self.write_offset += len as u64;
        self.live_bytes += len;
        self.max_pts = self.max_pts.max(frame.pts);
        self.insert_ordered(entry);
        self.evict_before(self.max_pts - self.capacity_ticks);
        self.maybe_compact();
    }

    fn clear(&mut self) {
        self.index.clear();
        self.live_bytes = 0;
        self.dead_bytes = 0;
        self.write_offset = 0;
        self.max_pts = i64::MIN;
        let _ = self.file.set_len(0);
    }

    fn set_capacity_seconds(&mut self, capacity_seconds: u32) {
        self.capacity_ticks = capacity_seconds.max(1) as i64 * self.ticks_per_sec;
        if self.max_pts != i64::MIN {
            self.evict_before(self.max_pts - self.capacity_ticks);
        }
    }

    fn len(&self) -> usize {
        self.index.len()
    }

    fn bytes(&self) -> usize {
        self.live_bytes
    }

    fn ticks_per_sec(&self) -> i64 {
        self.ticks_per_sec
    }

    fn capacity_seconds(&self) -> u32 {
        (self.capacity_ticks / self.ticks_per_sec.max(1)) as u32
    }

    fn buffered_seconds(&self) -> u32 {
        match (self.index.front(), self.index.back()) {
            (Some(f), Some(b)) => ((b.pts - f.pts) / self.ticks_per_sec.max(1)) as u32,
            _ => 0,
        }
    }

    fn newest_pts(&self) -> Option<Ticks> {
        self.index.back().map(|e| e.pts)
    }

    fn oldest_pts(&self) -> Option<Ticks> {
        self.index.front().map(|e| e.pts)
    }

    fn scan(&self) -> Box<dyn Iterator<Item = FrameMeta> + '_> {
        Box::new(self.index.iter().enumerate().map(|(index, e)| FrameMeta {
            index,
            pts: e.pts,
            is_keyframe: e.is_keyframe,
        }))
    }

    fn window(&self, start: usize, count: usize) -> Vec<EncodedFrame> {
        let mut out = Vec::with_capacity(count);
        for e in self.index.iter().skip(start).take(count) {
            let mut buf = vec![0u8; e.len as usize];
            if self.file.read_exact_at(&mut buf, e.offset).is_err() {
                // A read failure yields a short window rather than a panic; the
                // muxer will reject a truncated clip and surface a clear error.
                break;
            }
            out.push(EncodedFrame::new(
                Bytes::from(buf),
                e.is_keyframe,
                e.pts,
                e.dts,
            ));
        }
        out
    }
}

/// Default spill-file location for the disk replay buffer.
pub fn default_spill_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("open-recorder/replay-spill.bin")
}

/// Whether a spill file currently exists at `path` (diagnostic/tests).
pub fn spill_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FrameStore;
    use crate::MICROS_PER_SEC;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ord-disk-{}-{}-{}.bin",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn frame(sec: f64, keyframe: bool, fill: u8, len: usize) -> EncodedFrame {
        let pts = (sec * MICROS_PER_SEC as f64) as i64;
        EncodedFrame::new(vec![fill; len], keyframe, pts, pts)
    }

    #[test]
    fn push_window_roundtrips_payloads() {
        let mut s = DiskFrameStore::create(tmp("roundtrip"), 60, MICROS_PER_SEC).unwrap();
        s.push(frame(0.0, true, 0xAA, 8));
        s.push(frame(1.0, false, 0xBB, 4));
        s.push(frame(2.0, false, 0xCC, 6));
        assert_eq!(s.len(), 3);
        assert_eq!(s.bytes(), 18);

        let win = s.window(0, 3);
        assert_eq!(win.len(), 3);
        assert_eq!(&win[0].data[..], &[0xAA; 8]);
        assert_eq!(&win[1].data[..], &[0xBB; 4]);
        assert_eq!(&win[2].data[..], &[0xCC; 6]);
        assert!(win[0].is_keyframe);
        assert_eq!(win[1].pts, MICROS_PER_SEC);

        // A sub-window reads only the requested frames.
        let mid = s.window(1, 1);
        assert_eq!(mid.len(), 1);
        assert_eq!(&mid[0].data[..], &[0xBB; 4]);
    }

    #[test]
    fn eviction_matches_ring_semantics() {
        let mut s = DiskFrameStore::create(tmp("evict"), 5, MICROS_PER_SEC).unwrap();
        for i in 0..=10 {
            s.push(frame(i as f64, i == 0, i as u8, 10));
        }
        // Newest 10s, cutoff 5s: frames 5..=10 remain (6 frames).
        assert_eq!(s.len(), 6);
        assert_eq!(s.oldest_pts(), Some(5 * MICROS_PER_SEC));
        assert_eq!(s.newest_pts(), Some(10 * MICROS_PER_SEC));
        assert_eq!(s.bytes(), 60);
        // The materialized window is still the right payloads after eviction.
        let win = s.window(0, 6);
        assert_eq!(win.first().unwrap().pts, 5 * MICROS_PER_SEC);
        assert_eq!(&win[0].data[..], &[5u8; 10]);
    }

    #[test]
    fn out_of_order_push_kept_in_pts_order() {
        let mut s = DiskFrameStore::create(tmp("reorder"), 60, MICROS_PER_SEC).unwrap();
        s.push(frame(5.0, true, 0x55, 4));
        s.push(frame(3.0, false, 0x33, 4)); // earlier than newest
        assert_eq!(s.len(), 2);
        let metas: Vec<_> = s.scan().collect();
        assert_eq!(metas[0].pts, 3 * MICROS_PER_SEC);
        assert_eq!(metas[1].pts, 5 * MICROS_PER_SEC);
        // Window respects pts order even though payloads were appended reversed.
        let win = s.window(0, 2);
        assert_eq!(&win[0].data[..], &[0x33; 4]);
        assert_eq!(&win[1].data[..], &[0x55; 4]);
    }

    #[test]
    fn satisfies_frame_store_contract_via_trait_object() {
        let store: Box<dyn FrameStore> =
            Box::new(DiskFrameStore::create(tmp("dyn"), 60, MICROS_PER_SEC).unwrap());
        let mut store = store;
        store.push(frame(0.0, true, 1, 5));
        assert_eq!(store.len(), 1);
        assert_eq!(store.bytes(), 5);
        assert_eq!(store.newest_pts(), Some(0));
        assert_eq!(store.window(0, 1)[0].data.len(), 5);
    }

    #[test]
    fn clear_resets_and_truncates() {
        let path = tmp("clear");
        let mut s = DiskFrameStore::create(&path, 60, MICROS_PER_SEC).unwrap();
        s.push(frame(0.0, true, 1, 100));
        s.push(frame(1.0, false, 2, 100));
        s.clear();
        assert_eq!(s.len(), 0);
        assert_eq!(s.bytes(), 0);
        assert!(s.window(0, 1).is_empty());
        // After clear, a fresh low-pts epoch is retained (eviction anchor reset).
        s.push(frame(0.0, true, 9, 3));
        assert_eq!(s.len(), 1);
        assert_eq!(s.oldest_pts(), Some(0));
    }

    #[test]
    fn spill_file_removed_on_drop() {
        let path = tmp("drop");
        {
            let mut s = DiskFrameStore::create(&path, 60, MICROS_PER_SEC).unwrap();
            s.push(frame(0.0, true, 1, 10));
            assert!(path.exists());
        }
        assert!(!path.exists(), "spill file should be cleaned up on drop");
    }
}
