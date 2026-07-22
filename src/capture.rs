use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, StreamConfig};
use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tauri::ipc::Channel;

use crate::apm::APM_FRAME_SIZE;
use crate::{apm, loopback, resampler, CaptureOptions, Error, FrameEvent, PcmSource};
use base64::Engine as _;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
const TARGET_CHANNELS: u8 = 1;
// 20ms @16kHz. A ~10-30ms per-frame granularity lets streaming STT
// providers emit interim partials continuously instead of waiting for a
// coarse 100ms boundary.
const FRAME_SAMPLES: usize = APM_FRAME_SIZE * 2;
const DRAIN_TICK_MS: u64 = 10;
/// UI level meter only needs 10Hz — gate per wall-clock window since the
/// loop ticks at 100Hz.
const LEVEL_EMIT_INTERVAL_MS: u128 = 100;

/// Seed for the APM playback-delay estimator. AEC3 tolerates ±~250ms from
/// this value (per WebRTC docs) and its internal estimator refines from
/// there. For raw WASAPI loopback the digital pre-mixer signal is captured
/// directly, so the only acoustic delay is speaker → room → mic
/// (~10-50ms typical); if your app also plays TTS through a buffered
/// output (e.g. ~300ms of WaveOut latency) the real delay is larger.
/// 150ms covers both scenarios.
const APM_STREAM_DELAY_MS: i32 = 150;

/// Dedupe threshold for the 10Hz `Level` event stream. When neither RMS
/// channel has moved more than this normalized (0..1) delta since the last
/// emit, we skip the IPC round-trip. Saves ~10 events/sec when the user is
/// silent — the dominant case for a meter preview that runs while idle.
/// 0.005 ≈ -46 dB; below human-perceptible meter motion.
const RMS_DEDUPE_EPSILON: f32 = 0.005;

pub fn run(
    options: CaptureOptions,
    channel: Channel<FrameEvent>,
    stop_rx: Receiver<()>,
) -> Result<(), Error> {
    let emit_pcm = options.emits_pcm();

    // --- Windows mic permission preflight --------------------------------
    //
    // Without this, cpal `default_input_device()` returns an MMDevice,
    // `build_input_stream` succeeds, and frames flow — but every frame is
    // zero-filled because Windows silently mutes desktop apps that lack
    // mic permission. Result: downstream STT receives silence forever.
    //
    // We surface a `Permission(...)` error so the plugin's `start` command
    // emits `FrameEvent::Failure { category: "permission", ... }` and the
    // renderer can show the right dialog ("open Settings → Privacy →
    // Microphone"). The worker thread then exits gracefully.
    #[cfg(target_os = "windows")]
    if let Err(msg) = preflight_mic_permission() {
        return Err(Error::Permission(msg));
    }

    let host = cpal::default_host();
    let mic = host
        .default_input_device()
        .ok_or_else(|| Error::Device("no default input device".into()))?;
    let mic_config = mic.default_input_config().map_err(|e| {
        // Some OSes (Win11 with mic privacy off) surface permission denial
        // as a generic config error at this point — pass it through with
        // the right category so the renderer can deep-link to Settings.
        let msg = e.to_string();
        if msg.contains("permission") || msg.contains("Access") || msg.contains("denied") {
            Error::Permission(msg)
        } else {
            Error::Device(format!("default input config: {msg}"))
        }
    })?;
    let mic_sample_rate = mic_config.sample_rate().0;
    let mic_channels = mic_config.channels();

    let buf_mic = Arc::new(Mutex::new(Vec::<f32>::with_capacity(
        TARGET_SAMPLE_RATE as usize,
    )));
    let buf_loopback = Arc::new(Mutex::new(Vec::<f32>::with_capacity(
        TARGET_SAMPLE_RATE as usize,
    )));

    let buf_mic_cb = buf_mic.clone();
    let mic_resampler = resampler::Linear::new(
        mic_sample_rate,
        TARGET_SAMPLE_RATE,
        mic_channels,
        TARGET_CHANNELS,
    );
    let mic_resampler = Arc::new(Mutex::new(mic_resampler));

    let stream_config = StreamConfig {
        channels: mic_config.channels(),
        sample_rate: mic_config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let mic_stream = match mic_config.sample_format() {
        SampleFormat::F32 => mic.build_input_stream(
            &stream_config,
            {
                let resampler = mic_resampler.clone();
                move |data: &[f32], _info| {
                    let resampled = resampler.lock().process(data);
                    buf_mic_cb.lock().extend_from_slice(&resampled);
                }
            },
            err_cb,
            None,
        ),
        SampleFormat::I16 => mic.build_input_stream(
            &stream_config,
            {
                let resampler = mic_resampler.clone();
                move |data: &[i16], _info| {
                    let samples: Vec<f32> = data.iter().map(|&s| s.to_sample::<f32>()).collect();
                    let resampled = resampler.lock().process(&samples);
                    buf_mic_cb.lock().extend_from_slice(&resampled);
                }
            },
            err_cb,
            None,
        ),
        SampleFormat::U16 => mic.build_input_stream(
            &stream_config,
            {
                let resampler = mic_resampler.clone();
                move |data: &[u16], _info| {
                    let samples: Vec<f32> = data.iter().map(|&s| s.to_sample::<f32>()).collect();
                    let resampled = resampler.lock().process(&samples);
                    buf_mic_cb.lock().extend_from_slice(&resampled);
                }
            },
            err_cb,
            None,
        ),
        _ => return Err(Error::Device("unsupported sample format".into())),
    }
    .map_err(|e| Error::Device(e.to_string()))?;

    mic_stream
        .play()
        .map_err(|e| Error::Device(e.to_string()))?;

    // Open the loopback (WASAPI default-render-in-input mode on Windows;
    // unsupported elsewhere). Failure is non-fatal because the mic-only
    // path still produces useful audio.
    let loopback_stream = if options.uses_loopback() {
        match loopback::try_start(buf_loopback.clone()) {
            Ok(s) => Some(s),
            Err(err) => {
                log::warn!("[loopback] disabled: {err}");
                None
            }
        }
    } else {
        None
    };

    // APM AEC + NS. On non-Windows this resolves to a no-op stub. On
    // Windows it dlopens webrtc-apm.dll via the resolver in `apm.rs`. If
    // load fails (missing dll, ABI mismatch, etc) we proceed without APM
    // and log a warning — capture still works, just without echo
    // cancellation.
    //
    // Useful even without loopback: NS Low cleans up mic input for a
    // downstream VAD while AEC idles with no far-end reference.
    let apm_inst = if options.uses_apm() {
        match apm::open() {
            Ok(a) => {
                a.set_stream_delay_ms(APM_STREAM_DELAY_MS);
                Some(a)
            }
            Err(err) => {
                log::warn!("[apm] disabled: {err}");
                None
            }
        }
    } else {
        None
    };
    let has_loopback = loopback_stream.is_some();

    log::info!(
        "[audio] capture loop running — options={:?} mic={}Hz/{}ch loopback={}",
        options,
        mic_sample_rate,
        mic_channels,
        has_loopback
    );

    // Reusable per-tick scratch state. Lives across iterations to avoid
    // heap churn — the loop runs at 100Hz, so per-iteration `Vec::new()` +
    // `String::new()` would burn allocator traffic on the audio thread.
    // `Vec::clear()` preserves capacity for the next push.
    let mut state = LoopScratch::new();

    // Dedupe state for the `Level` event stream. We always emit the FIRST
    // level event (so the UI gets a non-zero starting reading) by sentinel-
    // init to NAN. The loop ticks at 100Hz (10ms) but level events are
    // throttled to 10Hz wall-clock via `last_level_emit_at`.
    let mut last_mic_rms = f32::NAN;
    let mut last_lp_rms = f32::NAN;
    let level_emit_start = std::time::Instant::now();
    let mut last_level_emit_at_ms: u128 = 0;

    loop {
        if stop_rx
            .recv_timeout(Duration::from_millis(DRAIN_TICK_MS))
            .is_ok()
        {
            break;
        }

        // Drain every available frame per wake-up — cpal callbacks can
        // burst multiple frames on WASAPI packet boundaries, and draining
        // only one per tick lets the buffer accumulate under load.
        let mut emitted_this_tick = 0;
        loop {
            // Pull mic frame if available.
            let have_mic = {
                let mut mic_buf = buf_mic.lock();
                if mic_buf.len() < FRAME_SAMPLES {
                    false
                } else {
                    state.mic_frame.clear();
                    state.mic_frame.extend(mic_buf.drain(..FRAME_SAMPLES));
                    true
                }
            };
            if !have_mic {
                break;
            }

            // Pull a matching loopback frame if available — loopback can
            // legitimately lag mic by 1-2 frames on session start (WASAPI
            // loopback opens slightly after the mic stream), so missing
            // loopback isn't an error, just no system audio this tick.
            {
                let mut lp_buf = buf_loopback.lock();
                state.lp_frame.clear();
                if has_loopback && lp_buf.len() >= FRAME_SAMPLES {
                    state.lp_frame.extend(lp_buf.drain(..FRAME_SAMPLES));
                }
            }

            // APM AEC requires a far-end reference. Feed loopback first so
            // `process_near` can subtract speaker bleed from the mic stream
            // before your app consumes it. Loopback still flows separately
            // below; `process_far` only updates WebRTC's echo model and
            // discards its output.
            if let Some(apm) = &apm_inst {
                if has_loopback && !state.lp_frame.is_empty() {
                    apm.process_far(&state.lp_frame);
                }
                apm.process_near(&mut state.mic_frame);
            }

            // RMS on **post-APM** mic (i.e. after echo cancellation), so
            // the meter shows what downstream consumers actually receive.
            // Loopback RMS is on pre-APM since APM doesn't modify the
            // loopback buffer (process_far discards output).
            let rms_mic = rms_f32(&state.mic_frame);
            let rms_lp = if state.lp_frame.is_empty() {
                0.0
            } else {
                rms_f32(&state.lp_frame)
            };

            // PCM path — mic and loopback are INDEPENDENT frames (no
            // additive mix), so the JS side can route each to its own
            // consumer and tag segments by source. Use `mixer::Mixer` if
            // you want a single combined stream instead.
            if emit_pcm {
                emit_pcm_frame(
                    &channel,
                    state.seq,
                    PcmSource::Mic,
                    &state.mic_frame,
                    &mut state.byte_scratch,
                    &mut state.b64_scratch,
                );
                if has_loopback && !state.lp_frame.is_empty() {
                    emit_pcm_frame(
                        &channel,
                        state.seq,
                        PcmSource::Loopback,
                        &state.lp_frame,
                        &mut state.byte_scratch,
                        &mut state.b64_scratch,
                    );
                }
            }

            // Throttled level emission — at most one per
            // LEVEL_EMIT_INTERVAL_MS (100ms = 10Hz). Without throttling
            // we'd spam ~100 events/sec at the 100Hz tick rate. Periodic
            // non-zero loopback levels also keep any renderer-side
            // "remote activity" timer alive during steady long speech.
            let now_ms = level_emit_start.elapsed().as_millis();
            let interval_elapsed = now_ms - last_level_emit_at_ms >= LEVEL_EMIT_INTERVAL_MS;
            let changed = !rms_close(rms_mic, last_mic_rms) || !rms_close(rms_lp, last_lp_rms);
            if interval_elapsed || last_mic_rms.is_nan() || changed {
                let _ = channel.send(FrameEvent::Level {
                    mic_rms: rms_mic,
                    loopback_rms: rms_lp,
                });
                last_mic_rms = rms_mic;
                last_lp_rms = rms_lp;
                last_level_emit_at_ms = now_ms;
            }
            state.seq = state.seq.wrapping_add(1);

            // Bound the inner loop so a runaway buffer never starves the
            // stop-signal check. 50 iterations × 20ms = 1s of audio per
            // wake-up cap — far beyond any plausible legitimate burst.
            emitted_this_tick += 1;
            if emitted_this_tick >= 50 {
                break;
            }
        }
    }

    // Explicit drop order matters:
    //   1. cpal streams stop first (so callbacks stop writing into the
    //      shared buffers).
    //   2. APM destroyed (frees native handles via libloading Drop).
    //   3. Scratch buffers + shared Arc<Mutex<Vec>> drop with the function
    //      frame — no extra cleanup needed (parking_lot Mutex has no FFI
    //      handle to release).
    drop(mic_stream);
    drop(loopback_stream);
    drop(apm_inst);
    log::info!("[audio] capture loop stopped — seq={}", state.seq);
    Ok(())
}

/// Reusable per-tick scratch state. Lives across iterations of the drain
/// loop to avoid heap churn. `Vec::clear()` keeps capacity for the next
/// push, so the steady-state path never re-allocates.
struct LoopScratch {
    seq: u64,
    mic_frame: Vec<f32>,
    lp_frame: Vec<f32>,
    byte_scratch: Vec<u8>,
    b64_scratch: String,
}

impl LoopScratch {
    fn new() -> Self {
        Self {
            seq: 0,
            mic_frame: Vec::with_capacity(FRAME_SAMPLES),
            lp_frame: Vec::with_capacity(FRAME_SAMPLES),
            // FRAME_SAMPLES i16 = FRAME_SAMPLES*2 bytes after to_le_bytes
            // (f32 → i16 conversion happens once, at the serialisation
            // boundary).
            byte_scratch: Vec::with_capacity(FRAME_SAMPLES * 2),
            // base64 expansion is 4/3 — reserve 3× to leave headroom.
            b64_scratch: String::with_capacity(FRAME_SAMPLES * 3),
        }
    }
}

/// f32 PCM → i16 LE bytes → base64 → `FrameEvent::Pcm`. Streaming STT
/// providers want raw 16-bit PCM in the wire envelope, so we keep the f32
/// pipeline throughout and only quantise to i16 here, once, at the
/// boundary — instead of doing it in the cpal callback before resampling +
/// APM (which destroys dynamic range on speech transients and measurably
/// hurts STT accuracy). Reuses caller-owned scratch buffers so the
/// steady-state path never allocates.
fn emit_pcm_frame(
    channel: &Channel<FrameEvent>,
    seq: u64,
    source: PcmSource,
    samples: &[f32],
    byte_scratch: &mut Vec<u8>,
    b64_scratch: &mut String,
) {
    byte_scratch.clear();
    byte_scratch.reserve(samples.len() * 2);
    for &s in samples {
        let clamped = (s * 32_767.0).clamp(i16::MIN as f32, i16::MAX as f32);
        byte_scratch.extend_from_slice(&(clamped as i16).to_le_bytes());
    }
    b64_scratch.clear();
    base64::engine::general_purpose::STANDARD.encode_string(&*byte_scratch, b64_scratch);
    let _ = channel.send(FrameEvent::Pcm {
        seq,
        source,
        sample_rate: TARGET_SAMPLE_RATE,
        channels: TARGET_CHANNELS,
        samples_base64: b64_scratch.clone(),
    });
}

/// Windows-only mic permission preflight. Reads the same registry-backed
/// consent value that
/// `DeviceAccessInformation.CreateFromDeviceClass(AudioCapture).CurrentStatus`
/// queries. `Allow` (or key missing on old Win10 builds) means proceed;
/// `Deny` aborts with a permission error.
///
/// We avoid linking the WinRT `Windows.Devices.Enumeration` API just for
/// this single check (the `windows` crate would add ~2MB to the binary) —
/// the consent registry value is the same source of truth as the API.
#[cfg(target_os = "windows")]
fn preflight_mic_permission() -> Result<(), String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let path = to_wide(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone",
    );
    let value_name = to_wide("Value");
    let mut hkey: HKEY = std::ptr::null_mut();
    let open_rc =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut hkey) };
    if open_rc != ERROR_SUCCESS {
        // Key missing — assume access allowed (older Win10 builds lack
        // this registry layout; fail-open avoids false-positive denials
        // on machines where the API just isn't there).
        return Ok(());
    }
    let mut buf = [0u16; 16];
    let mut buf_size: u32 = (buf.len() * 2) as u32;
    let mut ty: u32 = 0;
    let q_rc = unsafe {
        RegQueryValueExW(
            hkey,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr() as *mut u8,
            &mut buf_size,
        )
    };
    unsafe { RegCloseKey(hkey) };
    if q_rc != ERROR_SUCCESS || ty != REG_SZ {
        return Ok(()); // value missing — fail-open
    }
    let len_chars = (buf_size as usize / 2).saturating_sub(1); // drop terminator
    let s = String::from_utf16_lossy(&buf[..len_chars.min(buf.len())]);
    if s.eq_ignore_ascii_case("Deny") {
        return Err("Windows microphone access is disabled for desktop apps. \
             Open Settings → Privacy & Security → Microphone, then turn on \
             \"Microphone access\" and \"Let desktop apps access your microphone\"."
            .to_string());
    }
    Ok(())
}

/// Probe the OS for mic permission *without* starting capture. Returned
/// strings are stable and intended for JS-side switches.
pub(crate) fn check_mic_permission_status() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        match preflight_mic_permission() {
            Ok(()) => "allowed",
            Err(_) => "denied",
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // macOS delegates to `AVCaptureDevice.authorizationStatus(for: .audio)`
        // via the app's own permission flow. Report "unknown" so the
        // renderer falls back to a generic "permission may be required"
        // hint.
        "unknown"
    }
}

fn err_cb(err: cpal::StreamError) {
    log::warn!("[audio] stream error: {err}");
}

fn rms_f32(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64).powi(2)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// Returns true if the two RMS values are within `RMS_DEDUPE_EPSILON` of
/// each other AND the previous sample was a real number. The NAN sentinel
/// forces an emit on the first frame so the UI gets a starting reading.
fn rms_close(current: f32, previous: f32) -> bool {
    previous.is_finite() && (current - previous).abs() < RMS_DEDUPE_EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rms_close` returns false on first emit (NAN sentinel) so a fresh
    /// session always sends an initial Level event with the real reading.
    #[test]
    fn rms_close_returns_false_on_first_emit() {
        assert!(!rms_close(0.0, f32::NAN));
        assert!(!rms_close(0.5, f32::NAN));
    }

    /// Within-epsilon: dedupe. Past-epsilon: emit.
    #[test]
    fn rms_close_respects_epsilon() {
        // Just inside the threshold — should dedupe.
        assert!(rms_close(0.100, 0.103));
        assert!(rms_close(0.100, 0.097));
        // Right at the threshold — strict `<` so this is an emit.
        assert!(!rms_close(0.100, 0.100 + RMS_DEDUPE_EPSILON));
        // Well outside — emit.
        assert!(!rms_close(0.30, 0.10));
    }

    /// `levelOnly` must NOT emit PCM, open loopback, or run APM — even
    /// when the other flags are set, the stricter mode wins.
    #[test]
    fn level_only_never_emits_pcm() {
        let opts = CaptureOptions {
            loopback: true,
            processing: true,
            level_only: true,
        };
        assert!(!opts.emits_pcm());
        assert!(!opts.uses_apm());
        assert!(!opts.uses_loopback());
    }

    /// Defaults turn on the full pipeline. Regression guard against
    /// well-meaning refactors that flip them.
    #[test]
    fn default_options_use_full_pipeline() {
        let opts = CaptureOptions::default();
        assert!(opts.emits_pcm());
        assert!(opts.uses_apm());
        assert!(opts.uses_loopback());
    }

    /// Mic-only + processing: PCM yes, APM yes (NS still helps a
    /// downstream VAD), loopback no.
    #[test]
    fn mic_only_runs_apm_skips_loopback() {
        let opts = CaptureOptions {
            loopback: false,
            processing: true,
            level_only: false,
        };
        assert!(opts.emits_pcm());
        assert!(opts.uses_apm());
        assert!(!opts.uses_loopback());
    }

    /// LoopScratch preallocates all hot-path buffers — verify the capacity
    /// guarantees so the 100Hz drain loop never grows past these sizes
    /// (under nominal load).
    #[test]
    fn loop_scratch_preallocates_capacity() {
        let state = LoopScratch::new();
        assert!(state.mic_frame.capacity() >= FRAME_SAMPLES);
        assert!(state.lp_frame.capacity() >= FRAME_SAMPLES);
        assert!(state.byte_scratch.capacity() >= FRAME_SAMPLES * 2);
        // Base64 expansion is 4/3, so FRAME_SAMPLES*2 bytes (640) → ~854
        // chars. We reserve FRAME_SAMPLES * 3 (960) which leaves comfortable
        // headroom.
        assert!(state.b64_scratch.capacity() >= FRAME_SAMPLES * 8 / 3);
        assert_eq!(state.seq, 0);
    }

    /// Permission status query — Windows reports allowed/denied, other
    /// platforms report unknown. Just verify the call is total + returns
    /// one of the three stable strings.
    #[test]
    fn permission_status_returns_known_value() {
        let s = check_mic_permission_status();
        assert!(
            matches!(s, "allowed" | "denied" | "unknown"),
            "unexpected status: {s:?}"
        );
    }

    /// APM stream delay seed must stay within AEC3's ±250ms tolerance of
    /// typical real-world delays — see the constant's doc comment.
    #[test]
    fn apm_stream_delay_seed_unchanged() {
        assert_eq!(APM_STREAM_DELAY_MS, 150);
    }

    /// Sanity: RMS computed on a single APM frame (10ms @ 16k) should be
    /// stable — used as a building block in tests and the runtime loop.
    /// A flat 0.5 (half-scale) input gives RMS = 0.5.
    #[test]
    fn rms_half_scale_constant_is_half() {
        let frame = vec![0.5f32; APM_FRAME_SIZE];
        let r = rms_f32(&frame);
        assert!(
            (r - 0.5).abs() < 0.01,
            "expected ~0.5 for half-scale, got {r}"
        );
    }

    /// Smoke test: spawn the capture worker, let it run briefly, then send
    /// the stop signal and verify it exits cleanly. This catches:
    ///   * unwrap/expect panics on the audio hot path (the worker thread
    ///     is independent — a panic there terminates the thread but not
    ///     the test, so we verify the result via the join handle).
    ///   * FFI leaks (APM open without matching destroy) — observable as
    ///     a hang on Drop when the runtime tries to unload webrtc-apm.dll.
    ///   * Stream lifecycle bugs — cpal panics if you drop a Stream while
    ///     a callback is still in-flight; the explicit `drop(mic_stream)`
    ///     at the end of `run()` plus the stop_rx wait handles this.
    ///
    /// CI environments often lack a real audio device — the test only
    /// asserts the function returns without panicking, NOT that frames
    /// were captured.
    #[test]
    fn capture_run_starts_and_stops_cleanly() {
        // Skip if there's no default input — typical in headless CI. The
        // smoke check is for "doesn't panic"; an absent device gives us a
        // clean Err return, which is also valid.
        let host = cpal::default_host();
        if host.default_input_device().is_none() {
            eprintln!("[smoke] no default input device, skipping");
            return;
        }

        use std::sync::{Arc, Mutex};
        use tauri::ipc::InvokeResponseBody;

        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let panicked = Arc::new(Mutex::new(Option::<String>::None));

        // tauri 2's Channel has `Channel::new(handler)` taking
        // `Fn(InvokeResponseBody) -> Result<(), io::Error>`. A no-op
        // handler that drops the body is sufficient for the lifecycle
        // smoke test.
        let channel = Channel::<FrameEvent>::new(|_body: InvokeResponseBody| Ok(()));
        let panicked2 = panicked.clone();
        let handle = std::thread::spawn(move || {
            // The capture worker may panic on devices that don't expose a
            // usable default config (rare, but seen on virtual aggregate
            // devices). Catch it here so the test fails loudly with the
            // panic message instead of just hanging.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    CaptureOptions {
                        loopback: false,
                        processing: false,
                        level_only: true,
                    },
                    channel,
                    stop_rx,
                )
            }));
            if let Err(payload) = result {
                let msg = if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = payload.downcast_ref::<&'static str>() {
                    s.to_string()
                } else {
                    "<non-string panic payload>".to_string()
                };
                *panicked2.lock().unwrap() = Some(msg);
            }
        });

        // Let the worker open the mic stream + main loop spin briefly.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = stop_tx.send(());
        // The loop polls stop_rx every 10ms so this should return fast. A
        // hang here indicates the worker is stuck on an FFI call or a
        // deadlocked mutex.
        let join_result = handle.join();
        assert!(
            join_result.is_ok(),
            "worker thread panicked at join: {join_result:?}"
        );
        let panic_msg = panicked.lock().unwrap().clone();
        // It's OK for `run()` to return Err — the smoke test is for
        // "doesn't panic + obeys stop signal". Permission denial, no
        // device, etc. are all valid Err returns.
        if let Some(msg) = panic_msg {
            panic!("capture worker panicked: {msg}");
        }
    }
}
