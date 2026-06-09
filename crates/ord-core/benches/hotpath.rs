//! Hot-path benchmarks: ring-buffer push and the save-path clip copy.
//!
//! These guard the two latency-critical operations AGENTS.md calls out. The
//! `take_clip` bench is the headline for the `Bytes`-backed-frame change: with
//! `Vec<u8>` payloads it deep-copies the whole selected window (tens of MB) on
//! every save; with `bytes::Bytes` it is a set of refcount bumps. Run:
//!
//! ```sh
//! cargo bench -p ord-core --bench hotpath -- --save-baseline before
//! # ...make the change...
//! cargo bench -p ord-core --bench hotpath -- --baseline before
//! ```

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use ord_common::ClipDuration;
use ord_core::{EncodedFrame, MICROS_PER_SEC};
use ord_core::{Engine, MockBackend, RingBuffer};

/// A representative encoded frame size (~16 KiB ≈ 7.9 Mbps at 60 fps). Big enough
/// that a per-frame payload copy is measurable, small enough to keep the bench
/// fast.
const FRAME_BYTES: usize = 16 * 1024;
const FPS: u32 = 60;
const SECS: u32 = 30;

/// Pushing a full 30 s @ 60 fps window into a fresh evicting ring.
fn bench_ring_push(c: &mut Criterion) {
    let total = (FPS * SECS) as usize;
    let step = MICROS_PER_SEC / FPS as i64;
    let frames: Vec<EncodedFrame> = (0..total)
        .map(|i| {
            let pts = i as i64 * step;
            EncodedFrame::new(
                vec![0u8; FRAME_BYTES],
                (i as u32).is_multiple_of(FPS),
                pts,
                pts,
            )
        })
        .collect();

    let mut group = c.benchmark_group("ring_push");
    group.throughput(Throughput::Elements(total as u64));
    group.bench_function("push_30s_60fps", |b| {
        b.iter_batched(
            || frames.clone(),
            |batch| {
                let mut ring = RingBuffer::new(SECS);
                for f in batch {
                    ring.push(f);
                }
                black_box(ring.len())
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

/// Selecting + copying the last 30 s into a `PreparedClip` (the save hot path).
fn bench_take_clip(c: &mut Criterion) {
    let total = FPS * SECS;
    let mut eng = Engine::new(
        MockBackend::new(FPS, total, FPS).with_frame_bytes(FRAME_BYTES),
        SECS,
    );
    eng.start().expect("mock start");
    eng.drain_available();

    let mut group = c.benchmark_group("take_clip");
    group.throughput(Throughput::Bytes((total as usize * FRAME_BYTES) as u64));
    let dur = ClipDuration::new(SECS).unwrap();
    group.bench_function("last_30s_16kib_frames", |b| {
        b.iter(|| black_box(eng.take_clip(black_box(dur)).expect("clip")))
    });
    group.finish();
}

criterion_group!(benches, bench_ring_push, bench_take_clip);
criterion_main!(benches);
