// WASAPI loopback capture via cpal 0.16.
//
// cpal 0.16 has built-in loopback support on Windows: opening an input
// stream on a render (output) device transparently sets the WASAPI flag
// `AUDCLNT_STREAMFLAGS_LOOPBACK`. See
// `cpal-0.16.0/src/host/wasapi/device.rs:561` which conditionally OR's it
// in when `data_flow == eRender && stream_flags has input`. No cpal
// feature flag is required.
//
// Implementation notes:
//   * Picks the default render endpoint (cpal's `default_output_device`).
//     Win11 desktop laptops nearly always route call / partner audio
//     through the single default device. A follow-up could iterate
//     `output_devices()` and mix endpoints for multi-device setups.
//   * Resamples to 16k mono f32 via the shared `resampler::Linear`.
//   * Pushes into a `Mutex<Vec<f32>>` that the capture loop drains every
//     10ms. f32 throughout the pipeline skips two precision-eating
//     i16↔f32 round trips (cpal callback + APM) that measurably hurt STT
//     accuracy.
//
// Non-Windows: stub. On macOS the system-audio equivalent is
// ScreenCaptureKit, which is an entirely different (non-cpal) pipeline —
// out of scope for this plugin. Returning an error keeps `capture::run`
// from waiting forever for loopback frames that never come; capture
// proceeds mic-only.

use parking_lot::Mutex;
use std::sync::Arc;

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{Sample, SampleFormat, StreamConfig};

    /// Holds a live cpal loopback stream. Dropping this struct stops the
    /// stream + frees the WASAPI client handle (cpal `Stream`'s Drop
    /// calls `IAudioClient::Stop` and `IAudioRenderClient::Release`).
    #[allow(dead_code)] // diagnostic fields read via the Debug impl
    pub struct LoopbackStream {
        _stream: cpal::Stream,
        /// Sample rate of the underlying render endpoint — useful for
        /// diagnostics (capture.rs logs this so drift between mic-rate
        /// and loopback-rate is visible in the trace).
        pub source_sample_rate: u32,
        /// Channel count of the underlying render endpoint — typically
        /// 2 (stereo) on consumer hardware, occasionally 6 / 8 on
        /// surround setups. The resampler handles all of these via
        /// `chunks_exact(channels)` mixdown.
        pub source_channels: u16,
    }

    pub fn try_start(buf: Arc<Mutex<Vec<f32>>>) -> anyhow::Result<LoopbackStream> {
        let host = cpal::default_host();
        // Windows WASAPI loopback: query the default output device and
        // request an input stream on it. cpal 0.16 honors this via
        // `WasapiHost::default_output_device` + `build_input_stream`.
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("no default output device for WASAPI loopback"))?;
        let config = device
            .default_output_config()
            .map_err(|e| anyhow::anyhow!("default_output_config: {e}"))?;
        let sr = config.sample_rate();
        let ch = config.channels();
        let stream_config = StreamConfig {
            channels: ch,
            sample_rate: sr,
            buffer_size: cpal::BufferSize::Default,
        };

        let buf_cb = buf.clone();
        let mut resampler =
            crate::resampler::Linear::new(sr.0, crate::capture::TARGET_SAMPLE_RATE, ch, 1);

        log::info!(
            "[loopback] opening default render device — {} Hz / {} ch / {:?}",
            sr.0,
            ch,
            config.sample_format()
        );

        let stream = match config.sample_format() {
            SampleFormat::F32 => device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| {
                    let resampled = resampler.process(data);
                    buf_cb.lock().extend_from_slice(&resampled);
                },
                |err| log::warn!("[loopback] cpal err: {err}"),
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    let samples: Vec<f32> = data.iter().map(|&s| s.to_sample::<f32>()).collect();
                    let resampled = resampler.process(&samples);
                    buf_cb.lock().extend_from_slice(&resampled);
                },
                |err| log::warn!("[loopback] cpal err: {err}"),
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &stream_config,
                move |data: &[u16], _| {
                    let samples: Vec<f32> = data.iter().map(|&s| s.to_sample::<f32>()).collect();
                    let resampled = resampler.process(&samples);
                    buf_cb.lock().extend_from_slice(&resampled);
                },
                |err| log::warn!("[loopback] cpal err: {err}"),
                None,
            ),
            other => anyhow::bail!("unsupported loopback sample format: {other:?}"),
        }
        .map_err(|e| anyhow::anyhow!("build_input_stream (loopback): {e}"))?;
        stream
            .play()
            .map_err(|e| anyhow::anyhow!("loopback stream.play: {e}"))?;
        Ok(LoopbackStream {
            _stream: stream,
            source_sample_rate: sr.0,
            source_channels: ch,
        })
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::*;
    #[allow(dead_code)] // mirrors the Windows shape so call sites compile
    pub struct LoopbackStream {
        pub source_sample_rate: u32,
        pub source_channels: u16,
    }
    pub fn try_start(_buf: Arc<Mutex<Vec<f32>>>) -> anyhow::Result<LoopbackStream> {
        anyhow::bail!("loopback not supported on this platform")
    }
}

// `try_start` is the only public entry; `LoopbackStream` is the RAII handle
// whose `Drop` stops the underlying cpal stream. capture.rs holds it as
// `Option<LoopbackStream>` — the type is re-exported for callers that want
// to inspect `source_sample_rate` for diagnostics.
#[allow(unused_imports)]
pub use imp::{try_start, LoopbackStream};
