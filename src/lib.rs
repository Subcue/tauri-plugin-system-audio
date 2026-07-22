//! Dual audio capture for Tauri 2 — microphone + system audio (WASAPI
//! loopback) with WebRTC AEC3 echo cancellation.
//!
//! ```text
//!   ┌─────────────┐    ┌─────────────┐    ┌───────────┐
//!   │ mic capture │───▶│  resampler  │───▶│   APM     │──▶ FrameEvent::Pcm (mic)
//!   └─────────────┘    │  to 16k f32 │    │ near-end  │
//!                      └─────────────┘    └───────────┘
//!   ┌─────────────┐    ┌─────────────┐    ┌───────────┐
//!   │ loopback*   │───▶│  resampler  │───▶│   APM     │──▶ FrameEvent::Pcm (loopback)
//!   └─────────────┘    │  to 16k f32 │    │ reverse   │
//!      *Windows only    └─────────────┘    └───────────┘
//! ```
//!
//! Mic and loopback are emitted as **independent** 16 kHz mono PCM streams
//! (no additive mix), so the JS side can route each to its own consumer —
//! e.g. two STT sockets tagged "local speaker" vs "remote party". A
//! [`mixer::Mixer`] utility is included if you want a single combined
//! stream instead.
//!
//! On Windows the loopback (system output) feed doubles as the far-end
//! reference for WebRTC AEC3, so speaker bleed is cancelled from the mic
//! before your app ever sees it. On macOS the plugin runs mic-only:
//! Apple's voice-processing I/O (VPIO) already does AEC at the OS level,
//! and system-audio capture requires ScreenCaptureKit, which is out of
//! scope here.

pub mod apm;
mod capture;
mod loopback;
pub mod mixer;
pub mod resampler;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("audio session already running")]
    AlreadyRunning,
    #[error("audio session not running")]
    NotRunning,
    #[error("audio device error: {0}")]
    Device(String),
    /// OS denied microphone access. Surfaced as a distinct category so the
    /// renderer can show the right "open Settings" prompt vs. a generic
    /// "device unavailable" toast.
    #[error("audio permission denied: {0}")]
    Permission(String),
    #[error("io: {0}")]
    Io(String),
}

impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// What to capture and how to process it. All flags are orthogonal;
/// defaults give you the full pipeline (mic + loopback + APM).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureOptions {
    /// Capture the default render (output) device via WASAPI loopback.
    /// Windows only — on other platforms the flag is ignored and capture
    /// runs mic-only.
    pub loopback: bool,
    /// Run the WebRTC audio processing module (AEC3 echo cancellation +
    /// light noise suppression) on the mic path. Requires `webrtc-apm.dll`
    /// on Windows; a missing dll degrades gracefully to unprocessed mic.
    /// No-op on non-Windows platforms.
    pub processing: bool,
    /// Emit only 10 Hz [`FrameEvent::Level`] events — no PCM, no loopback,
    /// no APM. For "level meter preview" UI that runs while idle: ~0%
    /// upload cost, <1% CPU.
    pub level_only: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            loopback: true,
            processing: true,
            level_only: false,
        }
    }
}

impl CaptureOptions {
    pub(crate) fn uses_loopback(self) -> bool {
        self.loopback && !self.level_only
    }
    pub(crate) fn uses_apm(self) -> bool {
        self.processing && !self.level_only
    }
    pub(crate) fn emits_pcm(self) -> bool {
        !self.level_only
    }
}

/// Which physical source a PCM frame came from. Mic and loopback frames
/// arrive interleaved on the same channel; consumers split on this tag —
/// e.g. mic → "local speaker" STT, loopback → "remote party" STT.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PcmSource {
    Mic,
    Loopback,
}

/// Events emitted to the JS side over the Tauri [`Channel`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameEvent {
    /// One 20 ms frame of 16 kHz mono PCM, base64-encoded i16 LE bytes.
    Pcm {
        seq: u64,
        source: PcmSource,
        sample_rate: u32,
        channels: u8,
        samples_base64: String,
    },
    /// Throttled (10 Hz) RMS levels for meter UI, normalized 0..1.
    Level { mic_rms: f32, loopback_rms: f32 },
    /// Terminal failure of the capture worker. `category` is one of
    /// `"permission" | "device" | "io" | "lifecycle"`.
    Failure { category: String, message: String },
}

#[derive(Default)]
pub struct AudioSession {
    stop_token: Option<crossbeam_channel::Sender<()>>,
}

pub type SharedSession = Arc<Mutex<AudioSession>>;

#[tauri::command]
fn start(
    state: tauri::State<'_, SharedSession>,
    options: Option<CaptureOptions>,
    channel: Channel<FrameEvent>,
) -> Result<(), Error> {
    let mut session = state.lock();
    if session.stop_token.is_some() {
        return Err(Error::AlreadyRunning);
    }
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    session.stop_token = Some(tx);
    drop(session);

    let options = options.unwrap_or_default();
    std::thread::Builder::new()
        .name("system-audio".into())
        .spawn(move || {
            if let Err(err) = capture::run(options, channel.clone(), rx) {
                let category = match &err {
                    Error::Permission(_) => "permission",
                    Error::Device(_) => "device",
                    Error::Io(_) => "io",
                    Error::AlreadyRunning | Error::NotRunning => "lifecycle",
                };
                let _ = channel.send(FrameEvent::Failure {
                    category: category.into(),
                    message: err.to_string(),
                });
            }
        })
        .map_err(|e| Error::Io(e.to_string()))?;
    Ok(())
}

#[tauri::command]
fn stop(state: tauri::State<'_, SharedSession>) -> Result<(), Error> {
    let mut session = state.lock();
    if let Some(tx) = session.stop_token.take() {
        let _ = tx.send(());
        Ok(())
    } else {
        Err(Error::NotRunning)
    }
}

/// Probe the OS for mic permission **without** starting capture, so the UI
/// can grey out a Start button preemptively. Returns `"allowed"`,
/// `"denied"`, or `"unknown"` (platforms without a cheap probe).
#[tauri::command]
fn permission_status() -> &'static str {
    capture::check_mic_permission_status()
}

/// Initializes the plugin. Registers `start` / `stop` /
/// `permission_status` commands and, on Windows, resolves a bundled
/// `webrtc-apm.dll` through Tauri's resource resolver (works for
/// `tauri dev` and `tauri build` alike).
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("system-audio")
        .invoke_handler(tauri::generate_handler![start, stop, permission_status])
        .setup(|app, _api| {
            app.manage(SharedSession::default());
            #[cfg(target_os = "windows")]
            {
                use tauri::path::BaseDirectory;
                for candidate in ["webrtc-apm.dll", "resources/webrtc-apm.dll"] {
                    if let Ok(path) = app.path().resolve(candidate, BaseDirectory::Resource) {
                        if path.exists() {
                            apm::set_lib_path(path.to_string_lossy().into_owned());
                            break;
                        }
                    }
                }
            }
            Ok(())
        })
        .build()
}
