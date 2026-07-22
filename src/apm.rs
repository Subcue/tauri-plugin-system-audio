// WebRTC APM (AEC3 + NS + HPF + AGC2) — FFI binding to webrtc-apm.dll.
//
// Windows: loads the dll from one of the following candidates (first hit
// wins):
//   1. Explicit override via `set_lib_path("…\\webrtc-apm.dll")` — the
//      plugin's `setup` hook calls this with the Tauri resource-resolved
//      path when the app bundles the dll as a resource.
//   2. Plain name `webrtc-apm.dll` — works in dev when the dll sits next
//      to the .exe (`target/<profile>/webrtc-apm.dll`).
//   3. `<exe_dir>/webrtc-apm.dll` — explicit absolute path, same as (2)
//      but doesn't depend on Windows PATH search semantics.
//   4. `<exe_dir>/resources/webrtc-apm.dll` — Tauri's bundled-resource
//      layout after `tauri build`.
//
// Non-Windows: stub. On macOS, Apple's voice-processing I/O (VPIO)
// AudioUnit handles AEC + NS at the OS level, so shipping WebRTC APM there
// buys nothing.
//
// C ABI expected from the dll (see webrtc-apm/ABI.md in this repo):
//   * `webrtc_apm_create()` → `*mut Apm`
//   * `webrtc_apm_stream_config_create(sr: i32, num_channels: size_t)` → `*mut StreamConfig`
//   * `webrtc_apm_config_set_*(config, ...)` — all `i32` enabled flags, enums marshal as `i32`
//   * `webrtc_apm_process_stream(apm, src: float**, in_cfg, out_cfg, dst: float**)` → `ApmError(i32)`
//   * `webrtc_apm_process_reverse_stream(...)` — same signature
//   * `webrtc_apm_set_stream_delay_ms(apm, i32)`
//
// The `float**` is deinterleaved per-channel: array of channel-count pointers,
// each pointing to a contiguous block of `FRAME_SIZE` floats in [-1, 1].
// We're always mono (channels=1) so we pass a single-element `[*const f32; 1]`
// stack array; APM reads the one pointer, dereferences `FRAME_SIZE` floats.
// Frame size is exactly 160 samples (10ms @ 16kHz) — a hard APM constraint.

// Constants are only consumed by the Windows `imp` module; on other
// platforms the stub doesn't use them but we keep them defined at the
// public root so downstream callers (e.g. tests) can reference the
// canonical frame size regardless of host.
#[allow(dead_code)]
pub const APM_FRAME_SIZE: usize = 160;
#[allow(dead_code)]
pub const APM_SAMPLE_RATE: i32 = 16_000;

/// Default playback-delay seed. AEC3's delay estimator tolerates ±~250ms
/// of seed error and converges from there. For raw WASAPI loopback the
/// loopback path captures the digital pre-mixer signal, so the
/// speaker → room → mic round-trip is the only acoustic delay (~10-50ms
/// typical); buffered TTS playback paths can add ~300ms. 150ms covers
/// both scenarios.
#[allow(dead_code)]
pub const APM_PLAYBACK_DELAY_MS: i32 = 150;

#[cfg(target_os = "windows")]
mod imp {
    use super::{APM_FRAME_SIZE, APM_PLAYBACK_DELAY_MS, APM_SAMPLE_RATE};
    use libloading::{Library, Symbol};
    use parking_lot::Mutex;
    use std::env;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    // --- Native types ----------------------------------------------------
    //
    // All "config" / "stream_config" / "apm" pointers are opaque heap
    // handles owned by the dll. We round-trip them as `*mut c_void`.
    //
    // Enum values are passed as `i32`. For NoiseSuppression: Low=0
    // Moderate=1 High=2 VeryHigh=3. For DownmixMethod: AverageChannels=0
    // UseFirstChannel=1. GainControlMode: AdaptiveAnalog=0
    // AdaptiveDigital=1 FixedDigital=2.

    type ApmCreate = unsafe extern "C" fn() -> *mut c_void;
    type ApmDestroy = unsafe extern "C" fn(*mut c_void);
    type ApmInitialize = unsafe extern "C" fn(*mut c_void) -> i32;
    type ApmApplyConfig = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

    // Deinterleaved per-channel pointer arrays — see the module header.
    type ApmProcessStream = unsafe extern "C" fn(
        apm: *mut c_void,
        src: *const *const f32,
        input_cfg: *mut c_void,
        output_cfg: *mut c_void,
        dest: *const *mut f32,
    ) -> i32;
    type ApmProcessReverseStream = ApmProcessStream;

    type ApmSetStreamDelay = unsafe extern "C" fn(*mut c_void, i32);

    // size_t == usize on the Rust side.
    type ApmStreamConfigCreate = unsafe extern "C" fn(i32, usize) -> *mut c_void;
    type ApmStreamConfigDestroy = unsafe extern "C" fn(*mut c_void);

    type ApmConfigCreate = unsafe extern "C" fn() -> *mut c_void;
    type ApmConfigDestroy = unsafe extern "C" fn(*mut c_void);

    type ApmConfigSetEchoCanceller = unsafe extern "C" fn(*mut c_void, i32, i32);
    type ApmConfigSetNoiseSuppression = unsafe extern "C" fn(*mut c_void, i32, i32);
    type ApmConfigSetHighPassFilter = unsafe extern "C" fn(*mut c_void, i32);
    type ApmConfigSetGainController1 = unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32, i32);
    type ApmConfigSetGainController2 = unsafe extern "C" fn(*mut c_void, i32);
    type ApmConfigSetPipeline = unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32);

    pub struct Apm {
        _lib: Library,
        handle: *mut c_void,
        process_stream: ApmProcessStream,
        process_reverse: ApmProcessReverseStream,
        set_delay: ApmSetStreamDelay,
        destroy: ApmDestroy,
        cfg_destroy: ApmStreamConfigDestroy,
        stream_cfg: *mut c_void,
        // Reusable output buffer — APM frames are exactly APM_FRAME_SIZE
        // samples (10ms @ 16kHz). Both `process_near` and `process_far`
        // are called sequentially from the capture loop's drain tick,
        // so a single scratch suffices. Far-end output is discarded by
        // contract; near-end is copied back into the caller's frame.
        // Wrapped in `Mutex` for `Sync`.
        scratch_out: Mutex<Vec<f32>>,
    }
    unsafe impl Send for Apm {}
    unsafe impl Sync for Apm {}

    static LIB_PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();

    /// Override the default search candidates. The plugin's `setup` hook
    /// calls this with the path of the bundled resource — see `lib.rs`.
    pub fn set_lib_path(p: String) {
        let cell = LIB_PATH.get_or_init(|| Mutex::new(None));
        *cell.lock() = Some(p);
    }

    fn candidate_paths() -> Vec<String> {
        let mut out = Vec::new();
        if let Some(cell) = LIB_PATH.get() {
            if let Some(explicit) = cell.lock().clone() {
                out.push(explicit);
            }
        }
        out.push("webrtc-apm.dll".to_string());
        if let Ok(exe) = env::current_exe() {
            if let Some(dir) = exe.parent() {
                let exe_dir = PathBuf::from(dir);
                out.push(
                    exe_dir
                        .join("webrtc-apm.dll")
                        .to_string_lossy()
                        .into_owned(),
                );
                out.push(
                    exe_dir
                        .join("resources")
                        .join("webrtc-apm.dll")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        out
    }

    pub fn open() -> anyhow::Result<Apm> {
        let candidates = candidate_paths();
        let mut last_err: Option<String> = None;
        let lib = candidates
            .iter()
            .find_map(|p| match unsafe { Library::new(p) } {
                Ok(lib) => Some(lib),
                Err(e) => {
                    last_err = Some(format!("{p}: {e}"));
                    None
                }
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "webrtc-apm.dll not found (tried {} candidates) — last error: {}",
                    candidates.len(),
                    last_err.unwrap_or_else(|| "<none>".to_string())
                )
            })?;

        unsafe {
            let create: Symbol<ApmCreate> = lib.get(b"webrtc_apm_create")?;
            let init: Symbol<ApmInitialize> = lib.get(b"webrtc_apm_initialize")?;
            let apply: Symbol<ApmApplyConfig> = lib.get(b"webrtc_apm_apply_config")?;
            let cfg_create: Symbol<ApmStreamConfigCreate> =
                lib.get(b"webrtc_apm_stream_config_create")?;
            let cfg_destroy: Symbol<ApmStreamConfigDestroy> =
                lib.get(b"webrtc_apm_stream_config_destroy")?;
            let process_stream: Symbol<ApmProcessStream> = lib.get(b"webrtc_apm_process_stream")?;
            let process_reverse: Symbol<ApmProcessReverseStream> =
                lib.get(b"webrtc_apm_process_reverse_stream")?;
            let set_delay: Symbol<ApmSetStreamDelay> =
                lib.get(b"webrtc_apm_set_stream_delay_ms")?;
            let destroy: Symbol<ApmDestroy> = lib.get(b"webrtc_apm_destroy")?;

            // Config funcs. Note:
            // webrtc_apm_config_set_gain_controller1 takes (cfg, enabled,
            // mode, target_dbfs, gain_db, enable_limiter) — five int args
            // after the config handle.
            let apm_cfg_create: Symbol<ApmConfigCreate> = lib.get(b"webrtc_apm_config_create")?;
            let apm_cfg_destroy: Symbol<ApmConfigDestroy> =
                lib.get(b"webrtc_apm_config_destroy")?;
            let cfg_set_aec: Symbol<ApmConfigSetEchoCanceller> =
                lib.get(b"webrtc_apm_config_set_echo_canceller")?;
            let cfg_set_ns: Symbol<ApmConfigSetNoiseSuppression> =
                lib.get(b"webrtc_apm_config_set_noise_suppression")?;
            let cfg_set_hpf: Symbol<ApmConfigSetHighPassFilter> =
                lib.get(b"webrtc_apm_config_set_high_pass_filter")?;
            let cfg_set_agc1: Symbol<ApmConfigSetGainController1> =
                lib.get(b"webrtc_apm_config_set_gain_controller1")?;
            let cfg_set_agc2: Symbol<ApmConfigSetGainController2> =
                lib.get(b"webrtc_apm_config_set_gain_controller2")?;
            let cfg_set_pipeline: Symbol<ApmConfigSetPipeline> =
                lib.get(b"webrtc_apm_config_set_pipeline")?;

            let handle = create();
            if handle.is_null() {
                anyhow::bail!("webrtc_apm_create returned null");
            }

            let cfg = apm_cfg_create();
            if cfg.is_null() {
                (destroy)(handle);
                anyhow::bail!("webrtc_apm_config_create returned null");
            }
            // Tuned to mimic Apple VPIO's behavior baseline (a good
            // reference point for speech/STT workloads): AEC on, NS Low,
            // HPF off, AGC1/AGC2 off. WebRTC APM's aggressive defaults
            // (AGC2 + NS Moderate + HPF) re-pump room noise to -6dBFS
            // during pauses and shave the 80-300Hz speech fundamental —
            // both measurably hurt STT accuracy.
            cfg_set_aec(cfg, 1, 0);
            cfg_set_ns(cfg, 1, 0);
            cfg_set_hpf(cfg, 0);
            cfg_set_agc1(cfg, 0, 1, 3, 9, 1);
            cfg_set_agc2(cfg, 0);
            cfg_set_pipeline(cfg, APM_SAMPLE_RATE, 0, 0, 0);
            let apply_err = apply(handle, cfg);
            apm_cfg_destroy(cfg);
            if apply_err != 0 {
                (destroy)(handle);
                anyhow::bail!("webrtc_apm_apply_config returned {apply_err}");
            }
            let init_err = init(handle);
            if init_err != 0 {
                (destroy)(handle);
                anyhow::bail!("webrtc_apm_initialize returned {init_err}");
            }

            // 16kHz mono — matches the post-resample target shared by mic
            // and loopback paths in `capture.rs`.
            let stream_cfg = cfg_create(APM_SAMPLE_RATE, 1);
            if stream_cfg.is_null() {
                (destroy)(handle);
                anyhow::bail!("webrtc_apm_stream_config_create returned null");
            }
            // Seed playback delay; `capture.rs` may refine with
            // `set_stream_delay_ms` if it learns the actual loop latency.
            set_delay(handle, APM_PLAYBACK_DELAY_MS);

            Ok(Apm {
                process_stream: *process_stream,
                process_reverse: *process_reverse,
                set_delay: *set_delay,
                destroy: *destroy,
                cfg_destroy: *cfg_destroy,
                _lib: lib,
                handle,
                stream_cfg,
                scratch_out: Mutex::new(vec![0.0f32; APM_FRAME_SIZE]),
            })
        }
    }

    impl Apm {
        /// Process the near-end (mic) frame in place. Length must be a
        /// multiple of `APM_FRAME_SIZE`; remainder samples are passed
        /// through untouched. Samples are nominal `[-1.0, 1.0]` f32 —
        /// matches the cpal F32 capture format directly, no
        /// precision-eating i16 round-trip. Returns the last APM error
        /// code seen (0 = ok).
        pub fn process_near(&self, frame: &mut [f32]) -> i32 {
            let mut last: i32 = 0;
            let mut buf_out = self.scratch_out.lock();
            let mut i = 0;
            while i + APM_FRAME_SIZE <= frame.len() {
                let src_ptrs: [*const f32; 1] = [frame[i..].as_ptr()];
                let dst_ptrs: [*mut f32; 1] = [buf_out.as_mut_ptr()];
                unsafe {
                    last = (self.process_stream)(
                        self.handle,
                        src_ptrs.as_ptr(),
                        self.stream_cfg,
                        self.stream_cfg,
                        dst_ptrs.as_ptr(),
                    );
                }
                frame[i..i + APM_FRAME_SIZE].copy_from_slice(&buf_out[..APM_FRAME_SIZE]);
                i += APM_FRAME_SIZE;
            }
            last
        }

        /// Process the far-end (loopback / playback) frame for AEC
        /// reference. **Output is discarded** — APM uses the far-end only
        /// to maintain its echo model, the post-filter playback signal
        /// isn't useful to us. Length must be a multiple of
        /// `APM_FRAME_SIZE`; partial tail is ignored.
        pub fn process_far(&self, frame: &[f32]) -> i32 {
            let mut last: i32 = 0;
            let mut buf_out = self.scratch_out.lock();
            let mut i = 0;
            while i + APM_FRAME_SIZE <= frame.len() {
                let src_ptrs: [*const f32; 1] = [frame[i..].as_ptr()];
                let dst_ptrs: [*mut f32; 1] = [buf_out.as_mut_ptr()];
                unsafe {
                    last = (self.process_reverse)(
                        self.handle,
                        src_ptrs.as_ptr(),
                        self.stream_cfg,
                        self.stream_cfg,
                        dst_ptrs.as_ptr(),
                    );
                }
                i += APM_FRAME_SIZE;
            }
            last
        }

        pub fn set_stream_delay_ms(&self, ms: i32) {
            unsafe { (self.set_delay)(self.handle, ms) }
        }
    }

    impl Drop for Apm {
        fn drop(&mut self) {
            unsafe {
                (self.cfg_destroy)(self.stream_cfg);
                (self.destroy)(self.handle);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    pub struct Apm;
    pub fn open() -> anyhow::Result<Apm> {
        anyhow::bail!("APM not used on this platform (Apple VPIO handles AEC + NS on macOS)")
    }
    impl Apm {
        pub fn process_near(&self, _frame: &mut [f32]) -> i32 {
            0
        }
        pub fn process_far(&self, _frame: &[f32]) -> i32 {
            0
        }
        pub fn set_stream_delay_ms(&self, _ms: i32) {}
    }
    #[allow(dead_code)]
    pub fn set_lib_path(_p: String) {}
}

// `Apm` is the live FFI handle; `open` constructs one; `set_lib_path` is
// only invoked on Windows by the plugin's `setup` hook. The non-Windows
// stub keeps the same surface so call sites compile unchanged.
#[allow(unused_imports)]
pub use imp::{open, set_lib_path, Apm};

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: `open()` is the public entry point. On non-Windows it must
    /// error cleanly with a non-empty message (no panic, no UB). On
    /// Windows in test mode we may or may not find the dll depending on
    /// whether it sits next to the test binary, so we only assert the
    /// error message is a String.
    #[test]
    fn apm_open_returns_either_ok_or_err() {
        let res = open();
        match res {
            Ok(_apm) => {
                // If we got here we're on Windows with a working dll.
                // No further assertion — the smoke tests in capture.rs
                // exercise the real frames.
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(!msg.is_empty(), "error message must not be empty");
            }
        }
    }

    /// Verify the APM_FRAME_SIZE constant aligns with the upstream
    /// constraint (160 samples = 10ms @ 16kHz). This is hard-coded by
    /// webrtc-apm's internal block size; deviating produces BadDataLength.
    #[test]
    fn apm_frame_size_is_10ms_at_16khz() {
        assert_eq!(APM_FRAME_SIZE, 160);
        assert_eq!(APM_SAMPLE_RATE, 16_000);
        // 160 samples / 16000 Hz = 10ms exactly.
        let ms = (APM_FRAME_SIZE as f32 / APM_SAMPLE_RATE as f32) * 1000.0;
        assert!((ms - 10.0).abs() < 0.001, "frame must be 10ms, got {ms}ms");
    }

    /// On non-Windows the stub's `process_near` is a no-op pass-through —
    /// frame must come out unchanged.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn apm_stub_is_passthrough() {
        let apm = Apm;
        let mut frame = vec![0.25f32; APM_FRAME_SIZE];
        let original = frame.clone();
        let rc = apm.process_near(&mut frame);
        assert_eq!(rc, 0);
        assert_eq!(frame, original, "stub must not modify near-end frame");
    }
}
