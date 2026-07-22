# tauri-plugin-system-audio

**Capture what your app hears *and* what the computer plays — with echo cancellation — in Tauri 2.**

Microphone + system audio (WASAPI loopback) dual capture for Windows, with WebRTC **AEC3 echo cancellation**, anti-aliased resampling to 16 kHz mono PCM, and 10 Hz level metering. Extracted from the production desktop app of [SubcueAI](https://subcue.ai), where it feeds live dual-stream speech-to-text during video calls.

```text
  ┌─────────────┐    ┌─────────────┐    ┌───────────┐
  │ mic capture │───▶│  resampler  │───▶│   APM     │──▶ Pcm { source: "mic" }
  └─────────────┘    │  to 16k f32 │    │ near-end  │
                     └─────────────┘    └───────────┘
  ┌─────────────┐    ┌─────────────┐    ┌───────────┐
  │ loopback*   │───▶│  resampler  │───▶│   APM     │──▶ Pcm { source: "loopback" }
  └─────────────┘    │  to 16k f32 │    │ reverse   │
     *Windows only    └─────────────┘    └───────────┘
```

## Why this exists

Capturing **system audio** in a Tauri app is a recurring pain point: browsers can't do it, `getDisplayMedia` audio is unreliable, and most examples stop at "open a mic stream". This plugin packages the hard parts:

- **WASAPI loopback via cpal 0.16** — capturing the default *output* device as an input stream (`AUDCLNT_STREAMFLAGS_LOOPBACK`), so you hear Zoom/Meet/Teams/whatever the machine plays. No virtual audio driver, no Stereo Mix.
- **Echo cancellation that actually works** — the loopback feed doubles as WebRTC AEC3's far-end reference, so your own speakers are subtracted from the mic before your app sees it. Without this, speaker bleed re-enters the mic and wrecks downstream STT/recording.
- **Dual independent streams** — mic and loopback are emitted as separate tagged PCM streams (not premixed), so you can route them to two STT sessions and label "local speaker" vs "remote party". A sample-and-hold `Mixer` is included if you want one combined stream.
- **STT-grade signal path** — f32 end to end (no precision-eating i16 round-trips), 129-tap windowed-sinc anti-alias filter on downsample, i16 quantisation only at the serialisation boundary, 20 ms frames for continuous interim STT partials.
- **The boring-but-vital details** — Windows mic-consent registry preflight (otherwise you capture silence forever), permission-aware error categories for UI deep-links, allocation-free hot path, drift-tolerant frame pairing, clean stream teardown order.

**Platform behavior**: Windows = full pipeline. macOS/Linux = mic-only (loopback and APM compile to stubs; on macOS, Apple's VPIO already does AEC at the OS level, and system-audio capture there is ScreenCaptureKit's job — out of scope for this plugin).

## Install

Rust side (`src-tauri/Cargo.toml`):

```toml
[dependencies]
tauri-plugin-system-audio = { git = "https://github.com/Subcue/tauri-plugin-system-audio" }
```

Register the plugin:

```rust
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_system_audio::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

Allow the commands in your capability file (`src-tauri/capabilities/default.json`):

```json
{ "permissions": ["system-audio:default"] }
```

### Echo cancellation dll (Windows)

AEC needs [`webrtc-apm/webrtc-apm.dll`](webrtc-apm/) (a C-ABI build of WebRTC's AudioProcessing module, BSD-3 licensed — [ABI reference](webrtc-apm/ABI.md)). Copy it into `src-tauri/resources/` and bundle it:

```json
// tauri.conf.json
{ "bundle": { "resources": ["resources/webrtc-apm.dll"] } }
```

The plugin resolves the bundled dll automatically (dev *and* build). **Missing dll degrades gracefully**: capture keeps working, just without echo cancellation — a warning is logged.

## Use (JavaScript)

Copy [`guest-js/index.ts`](guest-js/index.ts) into your app:

```ts
import { start, stop, decodePcm } from './system-audio';

await start((event) => {
  switch (event.kind) {
    case 'pcm': {
      const samples = decodePcm(event.samples_base64); // Int16Array, 16 kHz mono, 20 ms
      if (event.source === 'mic') sttLocal.send(samples);
      else sttRemote.send(samples); // system audio: the remote party
      break;
    }
    case 'level': // 10 Hz meter: event.mic_rms / event.loopback_rms (0..1)
      break;
    case 'failure': // category: 'permission' | 'device' | 'io' | 'lifecycle'
      break;
  }
});

// later
await stop();
```

Options (all optional): `start(onEvent, { loopback: true, processing: true, levelOnly: false })`

- `loopback: false` — mic only.
- `processing: false` — skip WebRTC APM entirely.
- `levelOnly: true` — no PCM at all, just 10 Hz levels (~0% upload, <1% CPU) for an idle "mic check" meter.

Pre-flight the mic permission for your Settings UI:

```ts
const status = await permissionStatus(); // 'allowed' | 'denied' | 'unknown'
```

## Event reference

| Event | Payload | Notes |
|---|---|---|
| `pcm` | `seq`, `source` (`mic`\|`loopback`), `sample_rate` (16000), `channels` (1), `samples_base64` | 20 ms frames; i16 LE, base64 |
| `level` | `mic_rms`, `loopback_rms` (0..1) | ≤10 Hz, deduped below 0.005 delta |
| `failure` | `category`, `message` | Worker exited; `permission` → deep-link OS Settings |

## FAQ

**Why 16 kHz mono?** It's the native rate of speech models and WebRTC APM's processing band. Capturing at device rate and downsampling once (with a proper anti-alias FIR) beats letting each downstream consumer resample.

**Why base64 over a Tauri Channel instead of raw buffers?** One ordered channel carries tagged heterogeneous events (pcm/level/failure) with backpressure, ~160 KB/s per stream — trivial for the IPC. Decode cost is one `atob` per 20 ms.

**Can it capture a specific app's audio only?** No — WASAPI loopback captures the default render endpoint mix. Per-process capture needs the Windows 10 2004+ `AUDIOCLIENT_PROCESS_LOOPBACK` path, which cpal doesn't expose yet.

**Does AEC work if the user wears headphones?** There's simply no echo to cancel — AEC3 idles. Loopback capture still works and is unaffected.

**macOS system audio?** Use ScreenCaptureKit from native code (that's what SubcueAI's own macOS app does); this plugin intentionally stays cpal-only.

## Provenance & license

This is the audio pipeline that ships in [SubcueAI](https://subcue.ai)'s Windows desktop app (Tauri 2 + React), extracted with its production comments and tests intact.

[MIT](LICENSE) © 2026 [Subcue AI LLC](https://subcueai.com). `webrtc-apm.dll` is built from BSD-3-Clause WebRTC — see [THIRD-PARTY.md](THIRD-PARTY.md).
