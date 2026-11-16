export const PLAYBACK_RATES = [0.5, 1, 1.5, 2] as const;
export type PlaybackRate = (typeof PLAYBACK_RATES)[number];

let sessionMuted = false;
let sessionVolume = 1;

export function readSessionAudio(): { muted: boolean; volume: number } {
  return { muted: sessionMuted, volume: sessionVolume };
}

export function writeSessionAudio(muted: boolean, volume: number): void {
  sessionMuted = muted;
  sessionVolume = Math.max(0, Math.min(1, volume));
}

export function nextPlaybackRate(current: number): PlaybackRate {
  const idx = PLAYBACK_RATES.indexOf(current as PlaybackRate);
  return PLAYBACK_RATES[(idx + 1) % PLAYBACK_RATES.length];
}

export function parseStoredDuration(raw?: string): number {
  if (!raw) return 0;
  const parts = raw.split(':').map((p) => Number(p));
  if (parts.some((n) => !Number.isFinite(n) || n < 0)) return 0;
  if (parts.length === 2) return parts[0] * 60 + parts[1];
  if (parts.length === 3) return parts[0] * 3600 + parts[1] * 60 + parts[2];
  return 0;
}

export function formatVideoSummary(asset: {
  extension: string;
  width?: number;
  height?: number;
  metadata?: Record<string, string>;
}): string {
  return [
    asset.extension.toUpperCase(),
    asset.metadata?.['Video Codec'],
    asset.width && asset.height ? `${asset.width} × ${asset.height}` : undefined,
    asset.metadata?.Duration,
  ]
    .filter(Boolean)
    .join(' · ');
}

export function toggleStageFullscreen(
  stageEl: HTMLElement | undefined,
  videoEl: HTMLVideoElement | undefined,
): void {
  if (document.fullscreenElement) {
    void document.exitFullscreen().catch(() => undefined);
    return;
  }
  if (stageEl?.requestFullscreen) {
    void stageEl.requestFullscreen().catch(() => undefined);
    return;
  }
  const wkVideo = videoEl as (HTMLVideoElement & { webkitEnterFullscreen?: () => void }) | undefined;
  wkVideo?.webkitEnterFullscreen?.();
}
