import { Show, createEffect, createMemo, createSignal, on, onCleanup } from 'solid-js';
import { desktop } from './api';
import Icon from './Icon';
import { formatDurationSeconds, type Asset } from './model';
import { decodedThumbnail } from './PhotoView';
import { mediaUrl, previewUrl } from './preview-url';
import { Button } from './ui';
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
}) {
  let videoEl: HTMLVideoElement | undefined;
  const [src, setSrc] = createSignal('');
  const [poster, setPoster] = createSignal('');
  const [failed, setFailed] = createSignal(false);

  const assetKey = createMemo(() => `${props.asset.id}:${props.asset.preview_revision || 0}`);

  createEffect(
    on(assetKey, () => {
      let cancelled = false;
      const current = props.asset;
      setFailed(false);
      setSrc('');
      const memThumb = decodedThumbnail(current);
      if (memThumb) {
        setPoster(memThumb);
      } else {
        setPoster('');
        void Promise.resolve(previewUrl(current, 't512'))
          .then((resolvedPoster) => {
            if (!cancelled) setPoster(resolvedPoster);
          })
          .catch(() => undefined);
      }

      void Promise.resolve(mediaUrl(current))
        .then((resolvedSrc) => {
          if (!cancelled) setSrc(resolvedSrc);
        })
        .catch(() => {
          if (!cancelled) setFailed(true);
        });

      onCleanup(() => {
        cancelled = true;
      });
    }),
  );

  function handleLoadedMetadata() {
    if (!videoEl) return;
    const current = props.asset;
    const vw = videoEl.videoWidth || current.width;
    const vh = videoEl.videoHeight || current.height;
    const dur =
      !current.metadata?.Duration && Number.isFinite(videoEl.duration) && videoEl.duration > 0
        ? formatDurationSeconds(videoEl.duration)
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
    if (videoEl.paused) {
      void videoEl.play().catch(() => setFailed(true));
    } else {
      videoEl.pause();
    }
  }

  createEffect(() => {
    props.registerTogglePlay?.(togglePlay);
    onCleanup(() => props.registerTogglePlay?.(undefined));
  });

  const summaryLine = createMemo(() => {
    const parts = [
      props.asset.extension.toUpperCase(),
      props.asset.metadata?.['Video Codec'],
      props.asset.width && props.asset.height
        ? `${props.asset.width} × ${props.asset.height}`
        : undefined,
      props.asset.metadata?.Duration,
    ].filter(Boolean);
    return parts.join(' · ');
  });

  return (
    <div class="viewer video-viewer">
      <div class="photo-stage video-stage">
        <Show
          when={!failed()}
          fallback={
            <>
              <Show when={poster()}>
                {(url) => (
                  <img class="video-stage-backdrop" src={url()} alt="" aria-hidden="true" />
                )}
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
                controls
                playsinline
                preload="auto"
                aria-label={props.asset.filename}
                onLoadedMetadata={handleLoadedMetadata}
                onError={() => setFailed(true)}
              />
            )}
          </Show>
        </Show>

        <button
          class="viewer-nav previous icon-button"
          disabled={!props.hasPrevious}
          onClick={props.onPrevious}
          aria-label="上一张（←）"
        >
          <Icon name="left" size={22} />
        </button>
        <button
          class="viewer-nav next icon-button"
          disabled={!props.hasNext}
          onClick={props.onNext}
          aria-label="下一张（→）"
        >
          <Icon name="right" size={22} />
        </button>
      </div>
    </div>
  );
}
