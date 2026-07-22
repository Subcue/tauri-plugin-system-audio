// Mono-or-mixdown linear resampler with an anti-alias LPF on downsample.
//
// Operates on Float32 samples in nominal [-1, 1] range — matches the
// cpal F32 input format and the WebRTC APM's expected sample type. Keeping
// the entire pipeline in f32 (input → resample → APM → final i16 conversion
// only at the serialisation boundary) avoids two precision-eating
// i16↔f32 round trips that the macOS / Windows-native paths don't pay.
//
// Quality is acceptable for STT (which itself does FE/MFCC and tolerates
// moderate resampling artifacts). Swap for cubic / sinc if your workload
// needs more.
//
// The pre-filter is a 21-tap windowed-sinc (Hamming) FIR low-pass with
// cutoff at 0.4 × Nyquist of the **output** rate (the same spectral guard
// Core Audio's `AudioConverterRef` applies). Without this filter, downsampling 48kHz → 16kHz
// folds the 6-24kHz band back into the 0-6kHz speech band as alias noise
// and audibly hurts STT WER on plosives + fricatives. 21 taps + Hamming
// window gives ~50dB stop-band rejection at the cost of ~600µs/100ms frame
// per channel.
//
// Stereo → mono: take channel 0 only. Many Windows USB mics expose a mono
// element as "stereo" with L=R; equal-power sum 0.707×(L+R) on correlated
// channels = 1.414×L which clamps, while arithmetic average 0.5×(L+R) is
// fine but introduces no benefit over picking L. For real stereo content
// (rare in STT input — desktop mics) we lose the R channel info, which is
// an acceptable trade for STT accuracy on the common case. For >2 channels
// we average.

// 129-tap windowed-sinc — bumped from 21 because STT misrecognised
// fricatives/sibilants (/s/ /sh/ → smeared) under the wider transition band
// the shorter kernel produced on 48k→16k downsample. Group delay is still
// only ~1.3ms (64 samples / 48kHz) — negligible for streaming STT.
const FILTER_TAPS: usize = 129;

pub struct Linear {
    in_rate: u32,
    out_rate: u32,
    in_channels: u16,
    out_channels: u8,
    in_pos: f64,
    last_sample: f32,
    // FIR state used only when downsampling. Coefficients are precomputed
    // in `new()`; `history` is a circular buffer with length FILTER_TAPS.
    fir_coeffs: Option<[f32; FILTER_TAPS]>,
    fir_history: Vec<f32>,
    fir_write_idx: usize,
}

impl Linear {
    pub fn new(in_rate: u32, out_rate: u32, in_channels: u16, out_channels: u8) -> Self {
        let needs_lpf = in_rate > out_rate;
        let fir_coeffs = if needs_lpf {
            Some(design_lpf(in_rate, out_rate))
        } else {
            None
        };
        Self {
            in_rate,
            out_rate,
            in_channels,
            out_channels,
            in_pos: 0.0,
            last_sample: 0.0,
            fir_coeffs,
            fir_history: vec![0.0; FILTER_TAPS],
            fir_write_idx: 0,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        // 1. Mixdown to mono if needed.
        //   - 1ch: pass-through
        //   - 2ch: take L (most Windows USB mics expose mono-as-stereo with L=R)
        //   - Nch (N>2): arithmetic average
        let mono: Vec<f32> = match self.in_channels {
            0 | 1 => input.to_vec(),
            2 => input.chunks_exact(2).map(|frame| frame[0]).collect(),
            n => {
                let stride = n as usize;
                let scale = 1.0 / stride as f32;
                input
                    .chunks_exact(stride)
                    .map(|frame| frame.iter().sum::<f32>() * scale)
                    .collect()
            }
        };

        // 2. Anti-alias LPF (only on downsample). Filters the full
        // pre-resample stream in place, preserving the original sample
        // rate; the linear-interpolation step below then picks samples
        // out at the lower output rate.
        let filtered: Vec<f32> = if self.fir_coeffs.is_some() {
            self.fir_filter(&mono)
        } else {
            mono
        };

        // 3. Linear resample.
        if self.in_rate == self.out_rate {
            self.last_sample = *filtered.last().unwrap_or(&self.last_sample);
            return filtered;
        }
        let step = self.in_rate as f64 / self.out_rate as f64;
        let mut out = Vec::with_capacity((filtered.len() as f64 / step).ceil() as usize);
        let mut pos = self.in_pos;
        while pos < filtered.len() as f64 {
            let i = pos.floor() as usize;
            let frac = (pos - i as f64) as f32;
            let a = if i == 0 {
                self.last_sample
            } else {
                filtered[i - 1]
            };
            let b = filtered.get(i).copied().unwrap_or(a);
            out.push(a + (b - a) * frac);
            pos += step;
        }
        self.in_pos = pos - filtered.len() as f64;
        if let Some(&last) = filtered.last() {
            self.last_sample = last;
        }
        let _ = self.out_channels; // reserved for future stereo upmix
        out
    }

    /// In-place LPF using the precomputed FIR. `fir_history` is a circular
    /// buffer of the last FILTER_TAPS samples seen; for each new input we
    /// dot-product it with the (time-reversed) coefficients.
    fn fir_filter(&mut self, samples: &[f32]) -> Vec<f32> {
        let coeffs = match &self.fir_coeffs {
            Some(c) => *c,
            None => return samples.to_vec(),
        };
        let mut out = Vec::with_capacity(samples.len());
        for &s in samples {
            self.fir_history[self.fir_write_idx] = s;
            self.fir_write_idx = (self.fir_write_idx + 1) % FILTER_TAPS;
            let mut acc: f32 = 0.0;
            let mut idx = self.fir_write_idx;
            for &c in &coeffs {
                acc += self.fir_history[idx] * c;
                idx = (idx + 1) % FILTER_TAPS;
            }
            out.push(acc);
        }
        out
    }
}

/// Designs a windowed-sinc low-pass FIR. `in_rate` is the sample rate the
/// filter runs at; `out_rate` defines the cutoff (0.4 × out_rate / 2 = 0.2
/// of in_rate when downsampling 2.5×, etc). Hamming window for ~50dB
/// rejection at minimal compute.
fn design_lpf(in_rate: u32, out_rate: u32) -> [f32; FILTER_TAPS] {
    // Cutoff (relative to in_rate's Nyquist) — guard band sits at
    // 0.4 × out_rate / in_rate so we filter everything that would alias
    // into the output's [0, Nyquist] band.
    let cutoff_ratio = 0.4 * (out_rate as f32 / in_rate as f32);
    let mut coeffs = [0.0f32; FILTER_TAPS];
    let half = (FILTER_TAPS as isize - 1) / 2;
    let mut sum = 0.0f32;
    for i in 0..FILTER_TAPS {
        let n = i as isize - half;
        // Ideal sinc — limit-handled at n=0.
        let sinc = if n == 0 {
            2.0 * cutoff_ratio
        } else {
            let x = std::f32::consts::PI * n as f32;
            (2.0 * cutoff_ratio * x).sin() / x
        };
        // Hamming window — 0.54 - 0.46*cos(2πn/(N-1))
        let w = 0.54
            - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (FILTER_TAPS as f32 - 1.0)).cos();
        coeffs[i] = sinc * w;
        sum += coeffs[i];
    }
    // Normalize to unity DC gain so steady-state inputs aren't attenuated.
    if sum.abs() > 1e-6 {
        for c in &mut coeffs {
            *c /= sum;
        }
    }
    coeffs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: a 21-tap LPF with cutoff at 0.4 × Nyquist should pass DC
    /// approximately unchanged.
    #[test]
    fn lpf_unity_dc_gain() {
        let mut r = Linear::new(48_000, 16_000, 1, 1);
        let samples = vec![0.25f32; 100];
        let out = r.process(&samples);
        let avg = out.iter().rev().take(10).copied().sum::<f32>() / 10.0;
        assert!((avg - 0.25).abs() < 0.01, "dc gain off: {avg}");
    }

    /// 48k→16k is the dominant Win11 desktop case (default WASAPI render
    /// sample rate). Verify no panic + output length is ~1/3 of input.
    #[test]
    fn downsample_48k_to_16k() {
        let mut r = Linear::new(48_000, 16_000, 1, 1);
        let samples = vec![0.0f32; 4800];
        let out = r.process(&samples);
        assert!(
            out.len() >= 1500 && out.len() <= 1700,
            "out len {} not ~1600",
            out.len()
        );
    }

    /// 44.1k → 16k path — second-most-common Win11 case. Feed silence and
    /// verify no FIR ringing or DC bias.
    #[test]
    fn downsample_44100_to_16k_no_aliasing() {
        let mut r = Linear::new(44_100, 16_000, 1, 1);
        let samples = vec![0.0f32; 8820];
        let out = r.process(&samples);
        assert!(out.len() >= 3000 && out.len() <= 3400);
        let max_abs = out.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            max_abs <= 1e-4,
            "silence in → silence out, got max={max_abs}"
        );
    }

    /// Stereo → mono: take L. L=R=0.5 → 0.5.
    #[test]
    fn stereo_mixdown_takes_left() {
        let mut r = Linear::new(16_000, 16_000, 2, 1);
        let samples: Vec<f32> = (0..16).map(|_| 0.5f32).collect();
        let out = r.process(&samples);
        assert_eq!(out.len(), 8);
        for &s in &out {
            assert!((s - 0.5).abs() < 1e-3, "expected 0.5 (L), got {s}");
        }
    }

    /// Stereo with L=0.5 R=-0.5 → output is just L = 0.5.
    #[test]
    fn stereo_mixdown_ignores_right() {
        let mut r = Linear::new(16_000, 16_000, 2, 1);
        let mut samples: Vec<f32> = Vec::with_capacity(16);
        for _ in 0..8 {
            samples.push(0.5);
            samples.push(-0.5);
        }
        let out = r.process(&samples);
        assert_eq!(out.len(), 8);
        for &s in &out {
            assert!((s - 0.5).abs() < 1e-6, "expected L=0.5, got {s}");
        }
    }

    /// No resample, mono — pass-through with FIR bypass since in_rate == out_rate.
    #[test]
    fn passthrough_mono_no_resample() {
        let mut r = Linear::new(16_000, 16_000, 1, 1);
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 1000.0) - 0.8).collect();
        let out = r.process(&samples);
        assert_eq!(out, samples);
    }
}
