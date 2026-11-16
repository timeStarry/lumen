import { Show, batch, createEffect, createMemo, createSignal, on, onCleanup, onMount } from 'solid-js';
import { desktop, execute } from './api';
import Icon from './Icon';
import type { Asset, Preview } from './model';
import { mediaUrl, previewUrl } from './preview-url';
import { Button, VideoControls, VideoHeroPlay } from './ui';
import {
  clampSeekTime,
  formatVideoSummary,
  nextPlaybackRate,
  parseStoredDuration,
  readSessionAudio,
  syncVideoDimensionsAndDuration,
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
  let pendingSeek: number | null = null;

  const initialAudio = readSessionAudio();
  const [src, setSrc] = createSignal('');
  const [cover, setCover] = createSignal('');
  const [frameReady, setFrameReady] = createSignal(false);
  const [hasPlayed, setHasPlayed] = createSignal(false);
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
    if (rafId) cancelAnimationFrame(rafId);
    rafId = 0;
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
    if (bufferTimer) clearTimeout(bufferTimer);
    bufferTimer = undefined;
    setBuffering(false);
  };
  const syncAudioProps = (el: HTMLVideoElement) => {
    el.muted = muted();
    el.volume = volume();
    el.playbackRate = rate();
  };

  const assetKey = createMemo(() => `${props.asset.id}:${props.asset.preview_revision || 0}`);

  createEffect(
    on(assetKey, () => {
      let cancelled = false;
      const current = props.asset;
      pendingSeek = null;
      stopLoop();
      clearBuffer();
      batch(() => {
        setFailed(false);
        setPlaying(false);
        setFrameReady(false);
        setHasPlayed(false);
        setCover('');
        setCurrentTime(0);
        setDuration(parseStoredDuration(current.metadata?.Duration));
        setRate(1);
        setSrc('');
      });

      void execute<Preview>('asset.preview', { id: current.id, tier: 't1024' })
        .then(async (res) => {
          if (cancelled) return;
          if (res.asset) props.onInspected?.(res.asset);
          if (res.source !== 'video_slate') {
            const url = await previewUrl(res.asset ?? current, 't1024');
            if (!cancelled) setCover(url);
          }
        })
        .catch(() => undefined);

      void Promise.resolve(mediaUrl(current))
        .then((url) => !cancelled && setSrc(url))
        .catch(() => !cancelled && setFailed(true));

      onCleanup(() => {
        cancelled = true;
        pendingSeek = null;
        stopLoop();
        clearBuffer();
      });
    }),
  );

  function handleLoadedMetadata() {
    if (!videoEl) return;
    syncAudioProps(videoEl);
    if (Number.isFinite(videoEl.duration) && videoEl.duration > 0) setDuration(videoEl.duration);
    const updated = syncVideoDimensionsAndDuration(props.asset, videoEl.videoWidth, videoEl.videoHeight, videoEl.duration);
    if (updated) props.onInspected?.(updated);
  }

  function togglePlay() {
    if (!videoEl || failed()) return;
    if (videoEl.paused || videoEl.ended) {
      if (videoEl.ended) videoEl.currentTime = 0;
      syncAudioProps(videoEl);
      void videoEl.play().catch(() => setFailed(true));
    } else videoEl.pause();
  }

  function seekTo(seconds: number, commit: boolean) {
    if (!videoEl || failed()) return;
    const clamped = clampSeekTime(seconds, videoEl.duration, duration());
    setHasPlayed(true);
    setCurrentTime(clamped);
    if (videoEl.seeking && !commit) {
      pendingSeek = clamped;
      return;
    }
    pendingSeek = null;
    videoEl.currentTime = clamped;
  }

  function handleSeeked() {
    clearBuffer();
    setFrameReady(true);
    if (!videoEl) return;
    if (pendingSeek !== null) {
      const next = pendingSeek;
      pendingSeek = null;
      videoEl.currentTime = next;
      return;
    }
    setCurrentTime(videoEl.currentTime || 0);
  }

  function toggleMute() {
    const next = !muted();
    writeSessionAudio(next, volume());
    setMuted(next);
    if (videoEl) videoEl.muted = next;
  }

  function changeVolume(next: number) {
    const clamped = Math.max(0, Math.min(1, next));
    writeSessionAudio(clamped === 0, clamped);
    setVolume(clamped);
    setMuted(clamped === 0);
    if (videoEl) {
      videoEl.volume = clamped;
      videoEl.muted = clamped === 0;
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
    onCleanup(() => (props.registerTogglePlay?.(undefined), props.registerToggleMute?.(undefined)));
  });

  return (
    <div class="viewer video-viewer">
      <div ref={stageEl} class="photo-stage video-stage">
        <Show
          when={!failed()}
          fallback={
            <>
              <Show when={cover()}>{(url) => <img class="video-stage-backdrop" src={url()} alt="" aria-hidden="true" />}</Show>
              <div class="video-stage-fallback" role="status">
                <Icon name="player-play" size={28} />
                <p class="video-stage-fallback-title">当前内置播放器暂不支持此视频编码</p>
                <Show when={formatVideoSummary(props.asset)}>{(s) => <p class="video-stage-fallback-meta">{s()}</p>}</Show>
                <p class="video-stage-fallback-meta">
                  {desktop && !props.asset.source_key
                    ? '仍可正常执行星级、旗标、色标筛选与原片导出，或调用系统播放器查看。'
                    : '仍可正常执行星级、旗标、色标筛选，或导出原片后使用外部播放器查看。'}
                </p>
                <Show when={desktop && !props.asset.source_key && props.onOpenExternal}>
                  <Button variant="primary" size="sm" onClick={props.onOpenExternal}>
                    <Icon name="external-link" size={14} /> 使用外部播放器打开
                  </Button>
                </Show>
              </div>
            </>
          }
        >
          <Show when={!frameReady() && cover()}>
            {(url) => <img class="video-stage-cover" src={url()} alt="" aria-hidden="true" />}
          </Show>
          <Show when={src()}>
            {(mediaSrc) => (
              <video
                ref={videoEl}
                class="video-stage-player"
                src={mediaSrc()}
                playsinline
                disablepictureinpicture
                preload="auto"
                aria-label={props.asset.filename}
                onClick={togglePlay}
                onDblClick={handleFullscreen}
                onLoadedMetadata={handleLoadedMetadata}
                onLoadedData={() => setFrameReady(true)}
                onSeeked={handleSeeked}
                onDurationChange={() => videoEl && Number.isFinite(videoEl.duration) && videoEl.duration > 0 && setDuration(videoEl.duration)}
                onTimeUpdate={() => videoEl && !videoEl.seeking && setCurrentTime(videoEl.currentTime || 0)}
                onPlay={() => (clearBuffer(), setHasPlayed(true), setPlaying(true), startLoop())}
                onPlaying={() => (clearBuffer(), setFrameReady(true), setHasPlayed(true), setPlaying(true), startLoop())}
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
          <Show when={!playing() && !hasPlayed() && !buffering()}>
            <VideoHeroPlay onClick={togglePlay} />
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
