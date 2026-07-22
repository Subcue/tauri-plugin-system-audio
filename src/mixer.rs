// Sample-and-hold mixer for time-aligning mic + loopback streams.
//
// Background:
// On Windows we capture from two independent WASAPI streams (the user's
// mic + the default render device put into loopback mode). Each stream
// has its own period (CPAL `BufferSize::Default`), so even when both are
// nominally 16kHz mono post-resample, the rate they hand frames to our
// drain loop drifts. The 100ms aligning step in `capture.rs:run` can
// dequeue a 100ms mic frame paired with a "loopback frame that's actually
// 90ms of old samples + 10ms of empty waiting". Adding them together
// produces a phasey mix and bleeds mic-on-loopback into the AEC reference
// — AEC3 then over-cancels real mic content because it sees a delayed
// echo of itself in the reference.
//
// On macOS, ScreenCaptureKit-based pipelines don't need this:
// SCStream buffers + timestamps every CMSampleBuffer, and AVAudioEngine's
// tap shares the same audio HAL clock. CPAL has no such cross-stream sync, so we have to fake it.
//
// Fix: a small ring buffer holds the most recent ~10ms of each input.
// When `mix()` is called and one side is shorter than the other (because
// its WASAPI period hasn't elapsed yet), we pad with the most-recent
// captured sample (sample-and-hold) instead of zero. Removes the
// alias-by-zero-crossing artifact while staying simple.
//
// Output gains:
//   * mic_gain / loopback_gain default 0.7 each — leaves ~6dB of summed
//     headroom even with both streams near full-scale. Tunable per session.
//   * The summed `i16` sample is clamped to [-32768, 32767]; we do NOT
//     soft-clip with `tanh` because the downstream STT models train on
//     hard-clipped data anyway and tanh adds harmonic distortion.
//
// The output buffer is owned by the caller via `mix_into` (writes into a
// reusable `Vec<i16>` to avoid per-frame heap churn in the 10Hz capture
// loop). The convenience `mix(...) -> Vec<i16>` form exists for tests.

/// One 10ms APM frame at 16kHz. Drift past this threshold counts as
/// "macro-misalignment" (one stream skipped a beat); below it is normal
/// jitter between WASAPI periods.
const HOLD_SAMPLES: usize = 160;

pub struct Mixer {
    pub mic_gain: f32,
    pub loopback_gain: f32,
    last_mic: Option<i16>,
    last_loopback: Option<i16>,
    /// Counts iterations where mic vs loopback length difference exceeded
    /// `HOLD_SAMPLES`. Callers can read this to decide whether to nudge
    /// the APM stream delay (drift > 250ms beyond seed risks AEC3
    /// divergence — see `APM_PLAYBACK_DELAY_MS`).
    pub drift_frames: u64,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            mic_gain: 0.7,
            loopback_gain: 0.7,
            last_mic: None,
            last_loopback: None,
            drift_frames: 0,
        }
    }
}

impl Mixer {
    /// Allocate a fresh output buffer. Convenience for tests; the hot
    /// capture loop should use `mix_into` to recycle a `Vec<i16>`.
    /// `#[allow(dead_code)]` because production callers always use
    /// `mix_into` — keeping the alloc'ing helper for test ergonomics.
    #[allow(dead_code)]
    pub fn mix(&mut self, mic: &[i16], loopback: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(mic.len().max(loopback.len()));
        self.mix_into(mic, loopback, &mut out);
        out
    }

    /// Mix mic + loopback into the caller-supplied `out` buffer. Resizes
    /// `out` to `max(mic.len(), loopback.len())` and writes per-sample
    /// `(mic[i]*g_mic + loopback[i]*g_lp)` with sample-and-hold padding
    /// on the shorter side. Reusing `out` between calls is the entire
    /// point — the audio thread runs at 10Hz and cannot afford a 6.4KB
    /// alloc per tick.
    pub fn mix_into(&mut self, mic: &[i16], loopback: &[i16], out: &mut Vec<i16>) {
        let mic_gain = self.mic_gain;
        let lp_gain = self.loopback_gain;
        let len = mic.len().max(loopback.len());

        // Track drift for the next mix call's sample-and-hold tail.
        if let Some(&last) = mic.last() {
            self.last_mic = Some(last);
        }
        if let Some(&last) = loopback.last() {
            self.last_loopback = Some(last);
        }
        let mic_short = len.saturating_sub(mic.len());
        let lp_short = len.saturating_sub(loopback.len());
        if mic_short > HOLD_SAMPLES || lp_short > HOLD_SAMPLES {
            self.drift_frames = self.drift_frames.wrapping_add(1);
        }

        let mic_hold = self.last_mic.unwrap_or(0);
        let lp_hold = self.last_loopback.unwrap_or(0);

        out.clear();
        out.reserve(len);
        for i in 0..len {
            let a = mic.get(i).copied().unwrap_or(mic_hold) as f32 * mic_gain;
            let b = loopback.get(i).copied().unwrap_or(lp_hold) as f32 * lp_gain;
            let sum = (a + b).clamp(i16::MIN as f32, i16::MAX as f32);
            out.push(sum as i16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_and_hold_when_loopback_short() {
        let mut mixer = Mixer::default();
        // First mix establishes the "last value" for both streams.
        let _ = mixer.mix(&vec![100i16; 160], &vec![500i16; 160]);
        // Now mic delivers a full frame but loopback only 80 samples.
        let mic = vec![100i16; 160];
        let lp = vec![500i16; 80];
        let out = mixer.mix(&mic, &lp);
        assert_eq!(out.len(), 160);
        // The 80 trailing samples should use the held loopback value (500),
        // not zero — so the additive mix on tail samples is (100+500)*.7.
        let expected = ((100.0f32 + 500.0) * 0.7).round() as i16;
        assert_eq!(out[100], expected);
        assert_eq!(out[159], expected);
    }

    #[test]
    fn drift_counter_increments_on_large_misalignment() {
        let mut mixer = Mixer::default();
        // 100ms mic vs only 32 samples of loopback — short by 1568, way
        // over HOLD_SAMPLES (160).
        let out = mixer.mix(&vec![0i16; 1600], &vec![0i16; 32]);
        assert_eq!(out.len(), 1600);
        assert_eq!(mixer.drift_frames, 1);
    }

    /// Below-threshold drift (≤ HOLD_SAMPLES) should NOT count, because
    /// it's normal WASAPI period jitter, not a real desync.
    #[test]
    fn drift_counter_silent_on_jitter() {
        let mut mixer = Mixer::default();
        // 1600 vs 1500 — short by 100, well under HOLD_SAMPLES (160).
        let _ = mixer.mix(&vec![0i16; 1600], &vec![0i16; 1500]);
        assert_eq!(mixer.drift_frames, 0);
        // Repeat — still no drift.
        let _ = mixer.mix(&vec![0i16; 1600], &vec![0i16; 1550]);
        assert_eq!(mixer.drift_frames, 0);
    }

    /// Empty loopback (Mock mode, mic-only) — mix should still produce
    /// the mic frame, hold falls back to 0, additive becomes mic*gain.
    #[test]
    fn empty_loopback_yields_mic_only() {
        let mut mixer = Mixer::default();
        let mic = vec![1000i16; 1600];
        let out = mixer.mix(&mic, &[]);
        assert_eq!(out.len(), 1600);
        let expected = (1000.0 * 0.7) as i16;
        assert!(
            out.iter().all(|&s| (s - expected).abs() <= 1),
            "all samples should be ~700"
        );
    }

    /// Clip protection — gains of 0.7 + full-scale i16 = ~22937 each ×2 =
    /// 45874 → clamp to 32767. No wrap.
    #[test]
    fn additive_mix_clamps_at_i16_max() {
        let mut mixer = Mixer::default();
        let mic = vec![i16::MAX; 100];
        let lp = vec![i16::MAX; 100];
        let out = mixer.mix(&mic, &lp);
        // 0.7 * 32767 + 0.7 * 32767 = 45874 → clamp to 32767.
        for &s in &out {
            assert_eq!(s, i16::MAX);
        }
    }

    /// `mix_into` reuses the output buffer — verify it's resized in-place
    /// and old contents are overwritten (not appended).
    #[test]
    fn mix_into_reuses_buffer() {
        let mut mixer = Mixer::default();
        let mut out = vec![999i16; 100]; // pre-existing garbage
        let mic = vec![10i16; 1600];
        let lp = vec![20i16; 1600];
        mixer.mix_into(&mic, &lp, &mut out);
        assert_eq!(out.len(), 1600);
        // First sample should be ~21 (10*.7 + 20*.7 = 21), NOT 999.
        let expected = ((10.0 + 20.0) * 0.7) as i16;
        assert_eq!(out[0], expected);
        // Capacity should be preserved/grown — no shrink.
        assert!(out.capacity() >= 1600);
    }

    /// Drift counter should wrap rather than panic on overflow. Set it
    /// near u64::MAX and confirm one more drift event doesn't crash.
    #[test]
    fn drift_counter_wraps_safely() {
        let mut mixer = Mixer::default();
        mixer.drift_frames = u64::MAX;
        let _ = mixer.mix(&vec![0i16; 1600], &vec![0i16; 0]);
        assert_eq!(mixer.drift_frames, 0); // wrapping_add(1) on MAX = 0
    }
}
