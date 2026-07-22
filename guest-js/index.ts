/**
 * TypeScript bindings for tauri-plugin-system-audio.
 *
 * Copy this file into your app (or import it directly) — the plugin does
 * not currently publish an npm package.
 */
import { Channel, invoke } from '@tauri-apps/api/core';

export interface CaptureOptions {
  /** Capture the default output device via WASAPI loopback (Windows only). Default true. */
  loopback?: boolean;
  /** Run WebRTC APM (AEC3 echo cancellation + noise suppression) on the mic path. Default true. */
  processing?: boolean;
  /** Emit only 10 Hz level events — no PCM, no loopback, no APM. Default false. */
  levelOnly?: boolean;
}

export type FrameEvent =
  | {
      kind: 'pcm';
      seq: number;
      /** 'mic' = local microphone, 'loopback' = system audio (remote party). */
      source: 'mic' | 'loopback';
      /** Always 16000. */
      sample_rate: number;
      /** Always 1 (mono). */
      channels: number;
      /** Base64-encoded i16 little-endian PCM, 20 ms per frame. */
      samples_base64: string;
    }
  | { kind: 'level'; mic_rms: number; loopback_rms: number }
  | {
      kind: 'failure';
      category: 'permission' | 'device' | 'io' | 'lifecycle';
      message: string;
    };

/** Start capture. Events stream to `onEvent` until `stop()` is called. */
export async function start(
  onEvent: (event: FrameEvent) => void,
  options?: CaptureOptions,
): Promise<void> {
  const channel = new Channel<FrameEvent>();
  channel.onmessage = onEvent;
  await invoke('plugin:system-audio|start', { options, channel });
}

/** Stop a running capture session. Rejects if none is running. */
export async function stop(): Promise<void> {
  await invoke('plugin:system-audio|stop');
}

/**
 * Probe OS mic permission without starting capture.
 * 'unknown' on platforms without a cheap probe (macOS).
 */
export async function permissionStatus(): Promise<'allowed' | 'denied' | 'unknown'> {
  return invoke('plugin:system-audio|permission_status');
}

/** Decode one PCM frame's base64 payload into Int16 samples. */
export function decodePcm(samplesBase64: string): Int16Array {
  const raw = atob(samplesBase64);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
  return new Int16Array(bytes.buffer);
}
