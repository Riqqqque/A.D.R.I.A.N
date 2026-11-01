// Browser audio endpoint: registers this tab as an alarm speaker, listens on
// its SSE command channel, plays granted media URLs, and reports acks back.

import { post } from './api';

let source: EventSource | null = null;
let current: HTMLAudioElement | null = null;
let currentId = '';
let channelId = '';
let endpointId = '';

const LS_KEY = 'adrian.audio.endpoint';

export function audioEndpointId(): string {
  return endpointId;
}

export function audioEndpointStored(): boolean {
  return !!localStorage.getItem(LS_KEY);
}

function save() {
  localStorage.setItem(LS_KEY, JSON.stringify({ endpointId, channelId }));
}

/** Register (or resume) this browser tab as an audio endpoint. Requires a user
 * gesture so <audio> playback is allowed. */
export async function enableBrowserAudio(name: string): Promise<{ endpoint_id: string; channel_id: string }> {
  const stored = localStorage.getItem(LS_KEY);
  if (stored) {
    try {
      const s = JSON.parse(stored) as { endpointId: string; channelId: string };
      endpointId = s.endpointId;
      channelId = s.channelId;
      openChannel();
      return { endpoint_id: endpointId, channel_id: channelId };
    } catch {
      localStorage.removeItem(LS_KEY);
    }
  }
  const res = await post<{ endpoint_id: string; channel_id: string }>('/api/audio/register', { name });
  endpointId = res.endpoint_id;
  channelId = res.channel_id;
  save();
  openChannel();
  return res;
}

/** True while this tab has had no click or key press, as after the browser
 * restores it: autoplay may block its alarms until then. */
export function audioNeedsGesture(): boolean {
  return navigator.userActivation?.hasBeenActive === false;
}

/** Stop being a speaker. Resolves false when the server could not be told,
 * which leaves its endpoint enabled and a possible default alarm target. */
export async function disableBrowserAudio(): Promise<boolean> {
  const closing = { endpoint_id: endpointId, channel_id: channelId };
  localStorage.removeItem(LS_KEY);
  source?.close();
  source = null;
  current?.pause();
  current = null;
  currentId = '';
  endpointId = '';
  channelId = '';
  if (!closing.endpoint_id) return true;
  try {
    // A no-op once the endpoint has been removed.
    await post('/api/audio/unregister', closing);
    return true;
  } catch {
    return false;
  }
}

function openChannel() {
  source?.close();
  source = new EventSource(`/api/audio/channel/${channelId}`);
  source.addEventListener('cmd', (e) => {
    try {
      handle(JSON.parse((e as MessageEvent).data));
    } catch {
      /* malformed command */
    }
  });
  source.onerror = () => {
    /* EventSource auto-reconnects; server tolerates missing channel at delivery time */
  };
}

interface Cmd {
  event: string;
  delivery_id?: string;
  url?: string;
  volume?: number;
  label?: string;
}

async function handle(cmd: Cmd) {
  if (cmd.event === 'play' && cmd.url) {
    current?.pause();
    const audio = new Audio(cmd.url);
    audio.volume = Math.min(1, Math.max(0, cmd.volume ?? 0.7));
    current = audio;
    const id = cmd.delivery_id ?? '';
    currentId = id;
    audio.onended = () => void ack(id, true, 'played');
    audio.onerror = () => void ack(id, false, 'playback error');
    audio.play().catch((e) => void ack(id, false, `playback blocked: ${e}`));
  } else if (cmd.event === 'stop' && cmd.delivery_id === currentId) {
    // Stops reach every browser; only the one playing this delivery acts.
    current?.pause();
    current = null;
    currentId = '';
    void ack(cmd.delivery_id, true, 'stopped by user');
  }
}

async function ack(deliveryId: string, ok: boolean, result: string) {
  if (!deliveryId) return;
  try {
    await post('/api/audio/ack', { delivery_id: deliveryId, ok, result });
  } catch {
    /* best-effort */
  }
}
