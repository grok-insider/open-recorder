//! Pure audio-waveform peak math for the editor timeline.
//!
//! Bucketing is I/O-free so it unit-tests offline; the gui-only
//! [`crate::meta::extract_audio_peaks`] fills the samples via ffmpeg.

/// Collapse mono `f32` PCM into `n_peaks` max-abs peak values in `[0, 1]`.
pub fn peaks_from_samples(samples: &[f32], n_peaks: usize) -> Vec<f32> {
    if n_peaks == 0 {
        return Vec::new();
    }
    if samples.is_empty() {
        return vec![0.0; n_peaks];
    }
    let mut peaks = vec![0.0f32; n_peaks];
    let n = samples.len();
    for (i, &s) in samples.iter().enumerate() {
        let bucket = (i * n_peaks) / n;
        let a = s.abs();
        if a > peaks[bucket] {
            peaks[bucket] = a;
        }
    }
    // Normalize so the loudest peak reaches 1.0 (silent clips stay zero).
    let max = peaks.iter().copied().fold(0.0f32, f32::max);
    if max > 1e-6 {
        for p in &mut peaks {
            *p /= max;
        }
    }
    peaks
}

#[cfg(test)]
mod tests {
    use super::peaks_from_samples;

    #[test]
    fn empty_and_zero_peaks() {
        assert!(peaks_from_samples(&[], 0).is_empty());
        assert_eq!(peaks_from_samples(&[], 4), vec![0.0; 4]);
        assert!(peaks_from_samples(&[0.5], 0).is_empty());
    }

    #[test]
    fn normalizes_to_unit_and_keeps_shape() {
        // Four equal buckets: loudest in bucket 1.
        let samples = [0.1, 0.1, 0.8, 0.8, 0.2, 0.2, 0.4, 0.4];
        let p = peaks_from_samples(&samples, 4);
        assert_eq!(p.len(), 4);
        assert!((p[1] - 1.0).abs() < 1e-5);
        assert!(p[0] < p[1] && p[2] < p[1]);
        assert!(p.iter().all(|&x| (0.0..=1.0).contains(&x)));
    }

    #[test]
    fn silent_stays_zero() {
        assert_eq!(peaks_from_samples(&[0.0; 16], 4), vec![0.0; 4]);
    }
}
