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
//! that an *incremental* compaction reclaims (a bounded slice of live payload is
//! migrated per push; see [`COMPACT_BUDGET_BYTES`]), so disk use stays bounded
//! to roughly the live window while `push` latency stays bounded no matter how
//! large the window is. The hot path (`push`) does one positioned write, at
//! most one budgeted compaction slice, and no allocation beyond the metadata
//! entry.

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
/// in the spill file. During an in-flight compaction, `new_offset` records
/// where the payload has already been copied to in the replacement file.
#[derive(Debug, Clone, Copy)]
struct Entry {
    offset: u64,
    len: u32,
    pts: Ticks,
    dts: Ticks,
    is_keyframe: bool,
    new_offset: Option<u64>,
}

/// Compaction triggers once dead (evicted) payload bytes exceed this AND make up
/// more than half the file, so steady-state churn rewrites the file rarely.
const COMPACT_MIN_DEAD_BYTES: u64 = 32 * 1024 * 1024;

/// Per-push byte budget for incremental compaction copying. Bounds the extra
/// I/O any single `push` performs: the store contract says `push` sits on the
/// capture drain path and must never stall it, so the file rewrite is
/// amortized across pushes instead of running as one synchronous sweep (which
/// took seconds on a large window and overflowed the capture channel).
const COMPACT_BUDGET_BYTES: u64 = 4 * 1024 * 1024;

/// An in-flight incremental compaction: live payloads migrate into `tmp` a
/// budgeted slice per push; the swap happens only once every live entry has
/// been copied. Reads keep using the old file (old offsets stay authoritative)
/// until the swap, so `window` is never split across files.
struct Compaction {
    tmp_path: PathBuf,
    tmp: File,
    new_write_offset: u64,
}

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
    compact_min_dead_bytes: u64,
    compaction: Option<Compaction>,
    /// Frames dropped because a spill write failed (ENOSPC, dead disk). A
    /// growing count means the replay window is silently hollowing out.
    write_errors: u64,
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
            compact_min_dead_bytes: COMPACT_MIN_DEAD_BYTES,
            compaction: None,
            write_errors: 0,
        })
    }

    /// Tune when compaction starts (dead bytes threshold). Mainly for tests —
    /// the default only rewrites the file once ≥ 32 MiB are reclaimable.
    pub fn with_compact_min_dead_bytes(mut self, bytes: u64) -> Self {
        self.compact_min_dead_bytes = bytes;
        self
    }

    /// Frames dropped because their spill write failed (e.g. disk full). A
    /// nonzero, growing value means the replay window is hollowing out.
    pub fn write_errors(&self) -> u64 {
        self.write_errors
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

    /// Reclaim file space when evicted payloads dominate the file. The rewrite
    /// is *incremental*: each call copies at most [`COMPACT_BUDGET_BYTES`] of
    /// live payload into the replacement file, and the swap happens on the
    /// call that finishes the migration — `push` latency stays bounded no
    /// matter how large the window is.
    fn maybe_compact(&mut self) {
        if self.compaction.is_none()
            && (self.dead_bytes < self.compact_min_dead_bytes
                || self.dead_bytes * 2 <= self.write_offset)
        {
            return;
        }
        if let Err(e) = self.compact_step() {
            // Compaction is an optimization; on failure keep serving from the
            // existing (larger) file rather than risk losing the buffer.
            tracing::warn!(error = %e, "replay disk compaction failed; continuing");
            self.abort_compaction();
        }
    }

    fn compact_step(&mut self) -> io::Result<()> {
        if self.compaction.is_none() {
            let tmp_path = self.path.with_extension("compact");
            let tmp = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            for e in self.index.iter_mut() {
                e.new_offset = None;
            }
            self.compaction = Some(Compaction {
                tmp_path,
                tmp,
                new_write_offset: 0,
            });
        }
        let Some(c) = self.compaction.as_mut() else {
            return Ok(());
        };

        let mut budget = COMPACT_BUDGET_BYTES;
        let mut buf = Vec::new();
        let mut remaining = false;
        for e in self.index.iter_mut() {
            if e.new_offset.is_some() {
                continue;
            }
            if budget < e.len as u64 {
                remaining = true;
                break;
            }
            buf.resize(e.len as usize, 0);
            self.file.read_exact_at(&mut buf, e.offset)?;
            c.tmp.write_all_at(&buf, c.new_write_offset)?;
            e.new_offset = Some(c.new_write_offset);
            c.new_write_offset += e.len as u64;
            budget -= e.len as u64;
        }
        if remaining {
            return Ok(());
        }

        // Every live entry is migrated: swap the files. Entries evicted after
        // being copied leave dead bytes in the fresh file; account for them so
        // the next compaction cycle triggers correctly.
        let Some(c) = self.compaction.take() else {
            return Ok(());
        };
        c.tmp.sync_data().ok();
        std::fs::rename(&c.tmp_path, &self.path)?;
        self.file = c.tmp;
        for e in self.index.iter_mut() {
            if let Some(new) = e.new_offset.take() {
                e.offset = new;
            }
        }
        self.write_offset = c.new_write_offset;
        self.dead_bytes = c.new_write_offset.saturating_sub(self.live_bytes as u64);
        Ok(())
    }

    fn abort_compaction(&mut self) {
        if let Some(c) = self.compaction.take() {
            let _ = std::fs::remove_file(&c.tmp_path);
        }
        for e in self.index.iter_mut() {
            e.new_offset = None;
        }
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
            // A failed spill write must not panic the hot path; drop the frame
            // and count it so the hollowing window is observable.
            self.write_errors += 1;
            tracing::warn!(
                total = self.write_errors,
                "replay disk write failed; dropping frame"
            );
            return;
        }
        let entry = Entry {
            offset: self.write_offset,
            len: len as u32,
            pts: frame.pts,
            dts: frame.dts,
            is_keyframe: frame.is_keyframe,
            new_offset: None,
        };
        self.write_offset += len as u64;
        self.live_bytes += len;
        self.max_pts = self.max_pts.max(frame.pts);
        self.insert_ordered(entry);
        self.evict_before(self.max_pts - self.capacity_ticks);
        self.maybe_compact();
    }

    fn clear(&mut self) {
        self.abort_compaction();
        self.index.clear();
        self.live_bytes = 0;
        self.dead_bytes = 0;
        self.write_offset = 0;
        self.max_pts = i64::MIN;
        if let Err(e) = self.file.set_len(0) {
            tracing::warn!(error = %e, "could not truncate replay spill on clear");
        }
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
        // Payloads are appended in arrival order, so a save window is mostly
        // contiguous on disk: coalescing adjacent entries into one read cuts
        // syscalls by ~the GOP length, and `Bytes::slice` hands each frame a
        // zero-copy view of the shared buffer.
        const MAX_COALESCED_READ: u64 = 8 * 1024 * 1024;
        let entries: Vec<&Entry> = self.index.iter().skip(start).take(count).collect();
        let mut out = Vec::with_capacity(entries.len());
        let mut i = 0;
        while i < entries.len() {
            let run_offset = entries[i].offset;
            let mut run_len = entries[i].len as u64;
            let mut j = i + 1;
            while j < entries.len()
                && entries[j].offset == run_offset + run_len
                && run_len + entries[j].len as u64 <= MAX_COALESCED_READ
            {
                run_len += entries[j].len as u64;
                j += 1;
            }
            let mut buf = vec![0u8; run_len as usize];
            if self.file.read_exact_at(&mut buf, run_offset).is_err() {
                // A read failure yields a short window rather than a panic; the
                // muxer will reject a truncated clip and surface a clear error.
                break;
            }
            let shared = Bytes::from(buf);
            let mut off = 0usize;
            for e in &entries[i..j] {
                out.push(EncodedFrame::new(
                    shared.slice(off..off + e.len as usize),
                    e.is_keyframe,
                    e.pts,
                    e.dts,
                ));
                off += e.len as usize;
            }
            i = j;
        }
        out
    }
}

/// Default spill-file location for the disk replay buffer: the XDG runtime dir
/// (or the temp dir as a fallback) + `open-recorder/replay-spill.bin`.
pub fn default_spill_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("open-recorder/replay-spill.bin")
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
    fn compaction_reclaims_space_and_preserves_payloads() {
        let path = tmp("compact");
        let mut s = DiskFrameStore::create(&path, 5, MICROS_PER_SEC)
            .unwrap()
            .with_compact_min_dead_bytes(4 * 1024);
        // 1 KiB frames 1 s apart with a 5 s window: most of the file becomes
        // dead bytes, so the (lowered) threshold trips and the incremental
        // migration completes within the same pushes.
        for i in 0..100u32 {
            s.push(frame(i as f64, true, i as u8, 1024));
        }
        assert_eq!(s.len(), 6);
        let file_len = std::fs::metadata(&path).unwrap().len();
        assert!(
            file_len < 40 * 1024,
            "compaction never reclaimed space: file is {file_len} bytes"
        );
        // Every surviving payload is intact after the file swap.
        let win = s.window(0, s.len());
        assert_eq!(win.len(), 6);
        for (k, f) in win.iter().enumerate() {
            let fill = (94 + k) as u8;
            assert_eq!(&f.data[..], &[fill; 1024][..], "frame {k} corrupted");
        }
        assert_eq!(s.write_errors(), 0);
    }

    #[test]
    fn compaction_survives_clear_and_reuse() {
        let path = tmp("compact-clear");
        let mut s = DiskFrameStore::create(&path, 3, MICROS_PER_SEC)
            .unwrap()
            .with_compact_min_dead_bytes(1024);
        for i in 0..20u32 {
            s.push(frame(i as f64, true, i as u8, 512));
        }
        s.clear();
        assert_eq!(s.len(), 0);
        // The store keeps working after an aborted/afterwards-cleared cycle.
        s.push(frame(0.0, true, 0xEE, 256));
        let win = s.window(0, 1);
        assert_eq!(&win[0].data[..], &[0xEE; 256][..]);
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
