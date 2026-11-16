import { Show, createSignal } from 'solid-js';
import Icon from '../Icon';
import { formatDurationSeconds } from '../model';
import IconButton from './IconButton';
import Spinner from './Spinner';
import './VideoControls.css';

function Scrubber(props: {
  class: string;
  label: string;
  value: number;
  max: number;
  step: number;
  valueText: string;
  disabled?: boolean;
  onChange: (value: number, commit: boolean) => void;
}) {
  let railEl: HTMLSpanElement | undefined;
  const [dragging, setDragging] = createSignal(false);

  const safeMax = () => (Number.isFinite(props.max) && props.max > 0 ? props.max : 0);
  const clamped = () => {
    const m = safeMax();
    return m > 0 ? Math.max(0, Math.min(m, props.value || 0)) : 0;
  };
  const ratio = () => {
    const m = safeMax();
    return m > 0 ? clamped() / m : 0;
  };

  function valueAtClientX(clientX: number): number {
    const m = safeMax();
    if (!railEl || !m) return 0;
    const rect = railEl.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    const r = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    return r * m;
  }

  function handlePointerDown(event: PointerEvent & { currentTarget: HTMLDivElement }) {
    if (props.disabled || !safeMax() || event.button !== 0) return;
    event.preventDefault();
    event.stopPropagation();
    event.currentTarget.setPointerCapture(event.pointerId);
    setDragging(true);
    props.onChange(valueAtClientX(event.clientX), false);
  }

  function handlePointerMove(event: PointerEvent & { currentTarget: HTMLDivElement }) {
    if (!dragging() || !event.currentTarget.hasPointerCapture(event.pointerId)) return;
    event.preventDefault();
    props.onChange(valueAtClientX(event.clientX), false);
  }

  function finishDrag(event: PointerEvent & { currentTarget: HTMLDivElement }, commit: boolean) {
    if (!dragging()) return;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    setDragging(false);
    if (commit) props.onChange(valueAtClientX(event.clientX), true);
  }

  function handleKeyDown(event: KeyboardEvent) {
    if (props.disabled || !safeMax()) return;
    const m = safeMax();
    let next: number | null = null;
    if (event.key === 'ArrowRight' || event.key === 'ArrowUp') next = Math.min(m, clamped() + props.step);
    else if (event.key === 'ArrowLeft' || event.key === 'ArrowDown') next = Math.max(0, clamped() - props.step);
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = m;
    if (next !== null) {
      event.preventDefault();
      event.stopPropagation();
      props.onChange(next, true);
    }
  }

  return (
    <div
      class={`nl-scrubber ${props.class}${dragging() ? ' is-dragging' : ''}${props.disabled ? ' is-disabled' : ''}`}
      role="slider"
      tabIndex={props.disabled ? -1 : 0}
      aria-label={props.label}
      aria-valuemin={0}
      aria-valuemax={Number((safeMax() || 1).toFixed(2))}
      aria-valuenow={Number(clamped().toFixed(2))}
      aria-valuetext={props.valueText}
      aria-disabled={props.disabled || undefined}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={(e) => finishDrag(e, true)}
      onPointerCancel={(e) => finishDrag(e, false)}
      onKeyDown={handleKeyDown}
    >
      <span ref={railEl} class="nl-scrubber-rail" aria-hidden="true">
        <span class="nl-scrubber-fill-clip">
          <span class="nl-scrubber-fill" style={{ transform: `scaleX(${ratio()})` }} />
        </span>
        <span class="nl-scrubber-thumb" style={{ left: `${ratio() * 100}%` }} />
      </span>
    </div>
  );
}

export function VideoHeroPlay(props: { onClick: () => void }) {
  return (
    <button
      type="button"
      class="nl-video-hero-play"
      aria-label="播放视频（Space）"
      aria-keyshortcuts="Space"
      onClick={(event) => {
        event.stopPropagation();
        props.onClick();
      }}
    >
      <span class="nl-video-hero-play-glyph" aria-hidden="true">
        <Icon name="player-play" size={28} />
      </span>
    </button>
  );
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
  onSeek: (seconds: number, commit: boolean) => void;
  onToggleMute: () => void;
  onVolume: (volume: number) => void;
  onCycleRate: () => void;
  onToggleFullscreen: () => void;
}) {
  const safeDuration = () =>
    Number.isFinite(props.duration) && props.duration > 0 ? props.duration : 0;
  const clampedTime = () => {
    const dur = safeDuration();
    return dur > 0 ? Math.max(0, Math.min(dur, props.currentTime || 0)) : 0;
  };
  const effectiveVolume = () => (props.muted ? 0 : Math.max(0, Math.min(1, props.volume)));
  const wideTime = () => safeDuration() >= 3600;
  const timelineStep = () => Math.max(0.5, Math.min(5, (safeDuration() || 5) * 0.1));

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

      <Scrubber
        class="nl-video-timeline"
        label="视频播放进度"
        value={clampedTime()}
        max={safeDuration()}
        step={timelineStep()}
        disabled={!safeDuration()}
        valueText={`${formatDurationSeconds(clampedTime())} / ${formatDurationSeconds(safeDuration())}`}
        onChange={(seconds, commit) => props.onSeek(seconds, commit)}
      />

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

      <Scrubber
        class="nl-video-vol-wrap"
        label="音量"
        value={effectiveVolume()}
        max={1}
        step={0.05}
        valueText={`${Math.round(effectiveVolume() * 100)}%`}
        onChange={(vol) => props.onVolume(vol)}
      />

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
