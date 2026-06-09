//! Save-path mux latency: how long `write_clip` takes to stream-copy a clip to
//! an `.mkv` via ffmpeg. With `Bytes`-backed frames the selection (`take_clip`,
//! see `hotpath.rs`) is ~free, so this ffmpeg write is now the dominant save
//! cost — the AGENTS.md "save-path mux latency" guard.
//!
//! Gated behind the `mux` feature (needs the system ffmpeg libs). Run in the
//! devshell:
//!
//! ```sh
//! nix develop -c cargo bench -p ord-core --features mux --bench mux_latency
//! ```

#[cfg(feature = "mux")]
mod bench {
    use criterion::{black_box, Criterion};

    use ord_core::audio::{AudioCodec, AudioParams, EncodedAudioFrame};
    use ord_core::backend::{Codec, StreamParams, NANOS_PER_SEC};
    use ord_core::engine::PreparedClip;
    use ord_core::ring::EncodedFrame;

    /// One Annex-B access unit with valid SPS/PPS on keyframes (so `build_avcc`
    /// and the muxer accept it), mirroring the golden test's builder.
    fn access_unit(keyframe: bool) -> Vec<u8> {
        let sc = [0u8, 0, 0, 1];
        let mut d = Vec::new();
        if keyframe {
            d.extend_from_slice(&sc);
            d.extend_from_slice(&[0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01]);
            d.extend_from_slice(&sc);
            d.extend_from_slice(&[0x68, 0xce, 0x3c, 0x80]);
            d.extend_from_slice(&sc);
            d.extend_from_slice(&[0x65, 0x88, 0x84, 0x00, 0x33, 0x44, 0x55]);
        } else {
            d.extend_from_slice(&sc);
            d.extend_from_slice(&[0x41, 0x9a, 0x00, 0x10, 0x20]);
        }
        d
    }

    /// A 30 s @ 60 fps clip (keyframe every 1 s) with ~30 s of 20 ms Opus packets.
    fn clip_30s() -> PreparedClip {
        let step = NANOS_PER_SEC / 60;
        let frames: Vec<EncodedFrame> = (0..1800)
            .map(|i| {
                let kf = i % 60 == 0;
                EncodedFrame::new(access_unit(kf), kf, i as i64 * step, i as i64 * step)
            })
            .collect();
        let audio: Vec<EncodedAudioFrame> = (0..1500)
            .map(|i| EncodedAudioFrame::new(vec![0xfcu8; 400], i as i64 * 960, i as i64 * 20_000))
            .collect();
        PreparedClip {
            frames,
            audio,
            params: StreamParams {
                width: 2560,
                height: 1440,
                fps: 60,
                codec: Codec::H264,
                time_base_den: NANOS_PER_SEC,
            },
            audio_params: Some(AudioParams {
                sample_rate: 48_000,
                channels: 2,
                codec: AudioCodec::Opus,
            }),
        }
    }

    pub fn run() {
        let clip = clip_30s();
        let out = std::env::temp_dir().join(format!("ord-bench-mux-{}.mkv", std::process::id()));
        let mut c = Criterion::default().configure_from_args();
        let mut group = c.benchmark_group("mux");
        // ffmpeg writes dominate; fewer samples keep the bench quick.
        group.sample_size(20);
        group.bench_function("write_clip_30s_av", |b| {
            b.iter(|| {
                ord_core::write_clip(black_box(&clip), &out).expect("write_clip");
            })
        });
        group.finish();
        let _ = std::fs::remove_file(&out);
        c.final_summary();
    }
}

fn main() {
    #[cfg(feature = "mux")]
    bench::run();
    #[cfg(not(feature = "mux"))]
    eprintln!("mux_latency bench requires --features mux (skipped)");
}
