# webrtc-apm.dll — C ABI reference

`webrtc-apm.dll` is a thin C wrapper around WebRTC's
[AudioProcessing Module](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/)
(APM: AEC3 echo cancellation, noise suppression, high-pass filter, AGC1/AGC2).
The prebuilt x64 dll in this directory is what the plugin dlopens at runtime
on Windows. WebRTC is BSD-3-Clause licensed — see [`../THIRD-PARTY.md`](../THIRD-PARTY.md).

The plugin loads it via `libloading` and degrades gracefully (no echo
cancellation) when the dll is absent. You can also build your own wrapper —
any dll exporting this exact ABI works.

## Exported symbols

```c
// Lifecycle
void* webrtc_apm_create(void);
void  webrtc_apm_destroy(void* apm);
int   webrtc_apm_initialize(void* apm);
int   webrtc_apm_apply_config(void* apm, void* config);

// Config object
void* webrtc_apm_config_create(void);
void  webrtc_apm_config_destroy(void* config);
void  webrtc_apm_config_set_echo_canceller(void* config, int enabled, int mobile_mode);
void  webrtc_apm_config_set_noise_suppression(void* config, int enabled, int level);      // level: Low=0 Moderate=1 High=2 VeryHigh=3
void  webrtc_apm_config_set_high_pass_filter(void* config, int enabled);
void  webrtc_apm_config_set_gain_controller1(void* config, int enabled, int mode,        // mode: AdaptiveAnalog=0 AdaptiveDigital=1 FixedDigital=2
                                             int target_level_dbfs, int compression_gain_db,
                                             int enable_limiter);
void  webrtc_apm_config_set_gain_controller2(void* config, int enabled);
void  webrtc_apm_config_set_pipeline(void* config, int max_internal_rate,
                                     int multi_channel_render, int multi_channel_capture,
                                     int capture_downmix);                               // downmix: AverageChannels=0 UseFirstChannel=1

// Stream config (sample rate + channel count of the frames you pass)
void* webrtc_apm_stream_config_create(int sample_rate_hz, size_t num_channels);
void  webrtc_apm_stream_config_destroy(void* stream_config);

// Processing — src/dest are DEINTERLEAVED per-channel float pointer arrays:
// an array of `num_channels` pointers, each to a contiguous block of
// exactly 160 floats (10 ms @ 16 kHz) in [-1, 1].
int webrtc_apm_process_stream(void* apm, const float* const* src,
                              void* input_config, void* output_config,
                              float* const* dest);          // near-end (mic)
int webrtc_apm_process_reverse_stream(void* apm, const float* const* src,
                                      void* input_config, void* output_config,
                                      float* const* dest);  // far-end (playback reference)

void webrtc_apm_set_stream_delay_ms(void* apm, int delay_ms);
```

All `int` returns are `webrtc::AudioProcessing::Error` codes; `0` = success.

## Hard constraints

- **Frame size is exactly 160 samples** (10 ms @ 16 kHz). Any other length
  returns `BadDataLength`.
- Feed `process_reverse_stream` (far-end) **before** `process_stream`
  (near-end) within a tick so AEC3 has a fresh echo model.
- `set_stream_delay_ms` seeds the delay estimator; AEC3 tolerates ±~250 ms
  of seed error and converges from there.

## Building your own

Compile WebRTC's `modules/audio_processing` (or the
[`webrtc-audio-processing`](https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing)
standalone distribution) and export the wrapper functions above around an
`AudioProcessingBuilder` instance. The wrapper is ~200 lines of C++.
