import { Show, createEffect, createMemo, createSignal, on, onCleanup, onMount } from 'solid-js';
import { desktop } from './api';
import Icon from './Icon';
import { formatDurationSeconds, type Asset } from './model';
import { decodedThumbnail } from './PhotoView';
import { mediaUrl, previewUrl } from './preview-url';
import { Button, VideoControls } from './ui';
import {
  formatVideoSummary,
  nextPlaybackRate,
  parseStoredDuration,
  readSessionAudio,
  toggleStageFullscreen,
  writeSessionAudio,
} from './video-player';
import './VideoStage.css';

export default function VideoStage(props: {
  asset: Asset;
  hasPrevious: boolean;
  hasNext: boolean;
  onPrevious: () => void;
  onNext: () => void;
  onOpenExternal?: () => void;
  onInspected?: (asset: Asset) => void;
  registerTogglePlay?: (toggle: (() => void) | undefined) => void;
  registerToggleMute?: (toggle: (() => void) | undefined) => void;
}) {
  let stageEl: HTMLDivElement | undefined;
  let videoEl: HTMLVideoElement | undefined;
  let rafId = 0;
  let bufferTimer: ReturnType<typeof setTimeout> | undefined;

  const initialAudio = readSessionAudio();
  const [src, setSrc] = createSignal('');
  const [poster, setPoster] = createSignal('');
  const [failed, setFailed] = createSignal(false);
  const [playing, setPlaying] = createSignal(false);
  const [buffering, setBuffering] = createSignal(false);
  const [currentTime, setCurrentTime] = createSignal(0);
  const [duration, setDuration] = createSignal(parseStoredDuration(props.asset.metadata?.Duration));
  const [muted, setMuted] = createSignal(initialAudio.muted);
  const [volume, setVolume] = createSignal(initialAudio.volume);
  const [rate, setRate] = createSignal<number>(1);
  const [fullscreen, setFullscreen] = createSignal(false);

  const stopLoop = () => {
    if (rafId) {
      cancelAnimationFrame(rafId);
      rafId = 0;
    }
  };
  const startLoop = () => {
    stopLoop();
    const tick = () => {
      if (!videoEl || videoEl.paused || videoEl.ended || document.visibilityState === 'hidden') {
        rafId = 0;
        return;
      }
      setCurrentTime(videoEl.currentTime || 0);
      rafId = requestAnimationFrame(tick);
    };
    rafId = requestAnimationFrame(tick);
  };
  const clearBuffer = () => {
    if (bufferTimer) {
      clearTimeout(bufferTimer);
      bufferTimer = undefined;
    }
    setBuffering(false);
  };

  const assetKey = createMemo(() => `${props.asset.id}:${props.asset.preview_revision || 0}`);

  createEffect(
    on(assetKey, () => {
      let cancelled = false;
      const current = props.asset;
      stopLoop();
      clearBuffer();
      setFailed(false);
      setPlaying(false);
      setCurrentTime(0);
      setDuration(parseStoredDuration(current.metadata?.Duration));
      setRate(1);
      setSrc('');

      const memThumb = decodedThumbnail(current);
      if (memThumb) setPoster(memThumb);
      else {
        setPoster('');
        void Promise.resolve(previewUrl(current, 't512'))
          .then((url) => !cancelled && setPoster(url))
          .catch(() => undefined);
      }

      void Promise.resolve(mediaUrl(current))
        .then((url) => !cancelled && setSrc(url))
        .catch(() => !cancelled && setFailed(true));

      onCleanup(() => {
        cancelled = true;
        stopLoop();
        clearBuffer();
      });
    }),
  );

  function syncAudioProps(el: HTMLVideoElement) {
    el.muted = muted();
    el.volume = volume();
    el.playbackRate = rate();
  }

  function handleLoadedMetadata() {
    if (!videoEl) return;
    syncAudioProps(videoEl);
    const current = props.asset;
    const rawDur = videoEl.duration;
    if (Number.isFinite(rawDur) && rawDur > 0) setDuration(rawDur);
    const vw = videoEl.videoWidth || current.width;
    const vh = videoEl.videoHeight || current.height;
    const dur =
      !current.metadata?.Duration && Number.isFinite(rawDur) && rawDur > 0
        ? formatDurationSeconds(rawDur)
        : current.metadata?.Duration;
    if (vw !== current.width || vh !== current.height || dur !== current.metadata?.Duration) {
      props.onInspected?.({
        ...current,
        width: vw,
        height: vh,
        metadata: dur ? { ...current.metadata, Duration: dur } : current.metadata,
      });
    }
  }

  function togglePlay() {
    if (!videoEl || failed()) return;
    if (videoEl.paused || videoEl.ended) {
      if (videoEl.ended) videoEl.currentTime = 0;
      syncAudioProps(videoEl);
      void videoEl.play().catch(() => setFailed(true));
    } else videoEl.pause();
  }

  function seekTo(seconds: number) {
    if (!videoEl || failed()) return;
    const max = Number.isFinite(videoEl.duration) && videoEl.duration > 0 ? videoEl.duration : duration();
    const clamped = Math.max(0, Math.min(max || 0, seconds));
    videoEl.currentTime = clamped;
    setCurrentTime(clamped);
  }

  function toggleMute() {
    const next = !muted();
    writeSessionAudio(next, volume());
    setMuted(next);
    if (videoEl) videoEl.muted = next;
  }

  function changeVolume(next: number) {
    const clamped = Math.max(0, Math.min(1, next));
    const autoMuted = clamped === 0;
    writeSessionAudio(autoMuted, clamped);
    setVolume(clamped);
    setMuted(autoMuted);
    if (videoEl) {
      videoEl.volume = clamped;
      videoEl.muted = autoMuted;
    }
  }

  function cycleRate() {
    const next = nextPlaybackRate(rate());
    setRate(next);
    if (videoEl) videoEl.playbackRate = next;
  }

  const handleFullscreen = () => toggleStageFullscreen(stageEl, videoEl);

  onMount(() => {
    const onFs = () => setFullscreen(!!document.fullscreenElement);
    const onVis = () => (document.visibilityState === 'hidden' ? stopLoop() : playing() && startLoop());
    document.addEventListener('fullscreenchange', onFs);
    document.addEventListener('visibilitychange', onVis);
    onCleanup(() => {
      document.removeEventListener('fullscreenchange', onFs);
      document.removeEventListener('visibilitychange', onVis);
      stopLoop();
      clearBuffer();
    });
  });

  createEffect(() => {
    props.registerTogglePlay?.(togglePlay);
    props.registerToggleMute?.(toggleMute);
    onCleanup(() => {
      props.registerTogglePlay?.(undefined);
      props.registerToggleMute?.(undefined);
    });
  });

  const summaryLine = createMemo(() => formatVideoSummary(props.asset));

  return (
    <div class="viewer video-viewer">
      <div ref={stageEl} class="photo-stage video-stage">
        <Show
          when={!failed()}
          fallback={
            <>
              <Show when={poster()}>
                {(url) => <img class="video-stage-backdrop" src={url()} alt="" aria-hidden="true" />}
              </Show>
              <div class="video-stage-fallback" role="status">
                <Icon name="player-play" size={28} />
                <p class="video-stage-fallback-title">当前内置播放器暂不支持此视频编码</p>
                <Show when={summaryLine()}>
                  <p class="video-stage-fallback-meta">{summaryLine()}</p>
                </Show>
                <p class="video-stage-fallback-meta">
                  {desktop && !props.asset.source_key
                    ? '仍可正常执行星级、旗标、色标筛选与原片导出，或调用系统播放器查看。'
                    : '仍可正常执行星级、旗标、色标筛选，或导出原片后使用外部播放器查看。'}
                </p>
                <Show when={desktop && !props.asset.source_key && props.onOpenExternal}>
                  <Button variant="primary" size="sm" onClick={props.onOpenExternal}>
                    <Icon name="external-link" size={14} />
                    使用外部播放器打开
                  </Button>
                </Show>
              </div>
            </>
          }
        >
          <Show when={src()}>
            {(mediaSrc) => (
              <video
                ref={videoEl}
                class="video-stage-player"
                src={mediaSrc()}
                poster={poster() || undefined}
                playsinline
                disablepictureinpicture
                preload="auto"
                aria-label={props.asset.filename}
                onClick={togglePlay}
                onDblClick={handleFullscreen}
                onLoadedMetadata={handleLoadedMetadata}
                onDurationChange={() =>
                  videoEl && Number.isFinite(videoEl.duration) && videoEl.duration > 0 && setDuration(videoEl.duration)
                }
                onTimeUpdate={() => videoEl && setCurrentTime(videoEl.currentTime || 0)}
                onPlay={() => (clearBuffer(), setPlaying(true), startLoop())}
                onPlaying={() => (clearBuffer(), setPlaying(true), startLoop())}
                onPause={() => (clearBuffer(), setPlaying(false), stopLoop(), videoEl && setCurrentTime(videoEl.currentTime || 0))}
                onEnded={() => (clearBuffer(), setPlaying(false), stopLoop(), videoEl && Number.isFinite(videoEl.duration) && setCurrentTime(videoEl.duration))}
                onWaiting={() => {
                  if (!bufferTimer) bufferTimer = setTimeout(() => setBuffering(true), 220);
                }}
                onCanPlay={clearBuffer}
                onError={() => (clearBuffer(), stopLoop(), setPlaying(false), setFailed(true))}
              />
            )}
          </Show>
          <VideoControls
            playing={playing()}
            buffering={buffering()}
            currentTime={currentTime()}
            duration={duration()}
            muted={muted()}
            volume={volume()}
            rate={rate()}
            fullscreen={fullscreen()}
            onTogglePlay={togglePlay}
            onSeek={seekTo}
            onToggleMute={toggleMute}
            onVolume={changeVolume}
            onCycleRate={cycleRate}
            onToggleFullscreen={handleFullscreen}
          />
        </Show>

        <button class="viewer-nav previous icon-button" disabled={!props.hasPrevious} onClick={props.onPrevious} aria-label="上一张（←）">
          <Icon name="left" size={22} />
        </button>
        <button class="viewer-nav next icon-button" disabled={!props.hasNext} onClick={props.onNext} aria-label="下一张（→）">
          <Icon name="right" size={22} />
        </button>
      </div>
    </div>
  );
}
