import { Show } from 'solid-js';
import { formatDurationSeconds } from '../model';
import IconButton from './IconButton';
import Spinner from './Spinner';
import './VideoControls.css';

function guardMediaRangeKeys(event: KeyboardEvent) {
  const key = event.key.toLowerCase();
  if (
    event.key === ' ' ||
    /^[0-9]$/.test(key) ||
    key === '[' ||
    key === ']' ||
    key === 'p' ||
    key === 'x' ||
    key === 'u' ||
    key === 'm' ||
    key === 'd' ||
    key === 'i' ||
    key === 'escape'
  ) {
    // Prevent native range input from jumping to 0%..90% on digit keys so Viewer culling shortcuts fire cleanly.
    event.preventDefault();
  }
}

export default function VideoControls(props: {
  playing: boolean;
  buffering: boolean;
  currentTime: number;
  duration: number;
  muted: boolean;
  volume: number;
  rate: number;
  fullscreen: boolean;
  onTogglePlay: () => void;
  onSeek: (seconds: number) => void;
  onToggleMute: () => void;
  onVolume: (volume: number) => void;
  onCycleRate: () => void;
  onToggleFullscreen: () => void;
}) {
  const safeDuration = () =>
    Number.isFinite(props.duration) && props.duration > 0 ? props.duration : 0;
  const clampedTime = () => {
    const dur = safeDuration();
    if (!dur) return 0;
    return Math.max(0, Math.min(dur, props.currentTime || 0));
  };
  const progress = () => {
    const dur = safeDuration();
    return dur > 0 ? clampedTime() / dur : 0;
  };
  const effectiveVolume = () => (props.muted ? 0 : Math.max(0, Math.min(1, props.volume)));
  const wideTime = () => safeDuration() >= 3600;

  return (
    <div
      class={`nl-video-bar${props.playing ? ' is-playing' : ''}`}
      role="region"
      aria-label="视频播放控制"
      onClick={(e) => e.stopPropagation()}
      onDblClick={(e) => e.stopPropagation()}
    >
      <span class="nl-video-play-slot">
        <Show
          when={!props.buffering}
          fallback={
            <button
              type="button"
              class="nl-icon-btn nl-icon-btn--sm"
              aria-label="缓冲中，点击暂停（Space）"
              aria-keyshortcuts="Space"
              data-tooltip="缓冲中，点击暂停（Space）"
              data-kbd="Space"
              data-tip-side="top"
              onClick={props.onTogglePlay}
            >
              <Spinner size="sm" />
            </button>
          }
        >
          <IconButton
            size="sm"
            tipSide="top"
            tipAlign="start"
            icon={props.playing ? 'player-pause' : 'player-play'}
            label={props.playing ? '暂停（Space）' : '播放（Space）'}
            shortcut="Space"
            onClick={props.onTogglePlay}
          />
        </Show>
      </span>

      <span class={`nl-video-time${wideTime() ? ' nl-video-time--wide' : ''}`}>
        {formatDurationSeconds(clampedTime())}
      </span>

      <div class="nl-video-timeline">
        <span class="nl-video-track" aria-hidden="true">
          <span class="nl-video-fill" style={{ transform: `scaleX(${progress()})` }} />
        </span>
        <input
          class="nl-video-range"
          type="range"
          min={0}
          max={safeDuration() || 1}
          step="any"
          disabled={!safeDuration()}
          value={clampedTime()}
          aria-label="视频播放进度"
          onKeyDown={guardMediaRangeKeys}
          onInput={(event) => props.onSeek(Number(event.currentTarget.value))}
        />
      </div>

      <span class={`nl-video-time nl-video-time--dim${wideTime() ? ' nl-video-time--wide' : ''}`}>
        {formatDurationSeconds(safeDuration())}
      </span>

      <button
        type="button"
        class={`nl-video-rate${props.rate !== 1 ? ' active' : ''}`}
        aria-label={`播放倍速：当前 ${props.rate} 倍，点击切换`}
        title="切换播放倍速"
        onClick={props.onCycleRate}
      >
        {`${props.rate}×`}
      </button>

      <IconButton
        size="sm"
        tipSide="top"
        icon={props.muted || props.volume === 0 ? 'volume-off' : 'volume'}
        label={props.muted || props.volume === 0 ? '取消静音（M）' : '静音（M）'}
        shortcut="M"
        onClick={props.onToggleMute}
      />

      <div class="nl-video-vol-wrap">
        <span class="nl-video-track" aria-hidden="true">
          <span class="nl-video-fill" style={{ transform: `scaleX(${effectiveVolume()})` }} />
        </span>
        <input
          class="nl-video-range"
          type="range"
          min={0}
          max={1}
          step={0.02}
          value={effectiveVolume()}
          aria-label="音量"
          onKeyDown={guardMediaRangeKeys}
          onInput={(event) => props.onVolume(Number(event.currentTarget.value))}
        />
      </div>

      <IconButton
        size="sm"
        tipSide="top"
        icon={props.fullscreen ? 'arrows-minimize' : 'arrows-maximize'}
        tipAlign="end"
        label={props.fullscreen ? '退出全屏' : '全屏播放'}
        onClick={props.onToggleFullscreen}
      />
    </div>
  );
}
