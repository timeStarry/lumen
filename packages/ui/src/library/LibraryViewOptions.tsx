import { Show, createEffect, createSignal, onCleanup } from 'solid-js';
import { Portal } from 'solid-js/web';
import type { AssetListOptions } from '../model';
import { Button, IconButton, Select, Slider, StatusDot, Switch } from '../ui';

const VIEWPORT_GAP = 8;
const POPOVER_WIDTH = 272;

export const SORT_OPTIONS: { value: NonNullable<AssetListOptions['sort_by']>; label: string }[] = [
  { value: 'captured_at', label: '拍摄时间' },
  { value: 'created_at', label: '文件创建时间' },
  { value: 'modified_at', label: '文件修改时间' },
  { value: 'filename', label: '文件名' },
  { value: 'rating', label: '评级' },
];

export function sortSelectTitle(sort: NonNullable<AssetListOptions['sort_by']>): string | undefined {
  if (sort === 'captured_at') {
    return '按相机当地记录的拍摄时间排序；待索引或无有效时间的照片置后。';
  }
  if (sort === 'created_at') {
    return '源文件创建时间；未提供时间的照片置后。旧远端库可扫描更新以补充。';
  }
  return undefined;
}

/**
 * Compact view-presentation popover used when `.library-toolbar` has limited horizontal room.
 * Folds thumbnail size, burst stacking, hide-rejected, and (at very narrow widths) sort controls.
 */
export default function LibraryViewOptions(props: {
  includeSort: boolean;
  iconOnly?: boolean;
  sort: NonNullable<AssetListOptions['sort_by']>;
  onSort: (value: NonNullable<AssetListOptions['sort_by']>) => void;
  direction: NonNullable<AssetListOptions['sort_direction']>;
  onToggleDirection: () => void;
  burstStacks: boolean;
  onBurstStacks: (value: boolean) => void;
  hideRejected: boolean;
  hideRejectedLocked: boolean;
  onHideRejected: (value: boolean) => void;
  thumbSize: number;
  onThumbSize: (value: number) => void;
}) {
  let anchorEl: HTMLSpanElement | undefined;
  let popoverEl: HTMLDivElement | undefined;
  const [open, setOpen] = createSignal(false);
  const [coords, setCoords] = createSignal({ left: VIEWPORT_GAP, top: VIEWPORT_GAP });

  const customized = () => !props.burstStacks || props.hideRejected;

  function updateCoords() {
    const trigger = anchorEl?.querySelector<HTMLElement>('button');
    if (!trigger) return;
    const rect = trigger.getBoundingClientRect();
    const vw = window.innerWidth || document.documentElement.clientWidth || 1024;
    const vh = window.innerHeight || document.documentElement.clientHeight || 768;
    const width = popoverEl?.offsetWidth || POPOVER_WIDTH;
    const height = popoverEl?.offsetHeight || 220;
    const left = Math.max(VIEWPORT_GAP, Math.min(rect.right - width, vw - width - VIEWPORT_GAP));
    const below = rect.bottom + VIEWPORT_GAP;
    const top =
      below + height <= vh - VIEWPORT_GAP
        ? below
        : Math.max(VIEWPORT_GAP, rect.top - height - VIEWPORT_GAP);
    setCoords({ left: Math.round(left), top: Math.round(top) });
  }

  function closePopover(restoreFocus = false) {
    if (!open()) return;
    setOpen(false);
    if (restoreFocus) {
      anchorEl?.querySelector<HTMLElement>('button')?.focus({ preventScroll: true });
    }
  }

  createEffect(() => {
    if (!open()) return;
    updateCoords();
    requestAnimationFrame(() => updateCoords());
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node | null;
      if (anchorEl?.contains(target) || popoverEl?.contains(target)) return;
      if (target instanceof Element && target.closest('.nl-select-popover')) return;
      closePopover(false);
    };
    const onResize = () => updateCoords();
    const onScroll = (event: Event) => {
      if (popoverEl && event.target instanceof Node && popoverEl.contains(event.target)) return;
      if (event.target instanceof Element && event.target.closest('.nl-select-popover')) return;
      closePopover(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        if (document.querySelector('.nl-select-popover')) return;
        event.preventDefault();
        event.stopPropagation();
        closePopover(true);
      }
    };
    document.addEventListener('pointerdown', onPointerDown, true);
    window.addEventListener('resize', onResize);
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('keydown', onKeyDown);
    onCleanup(() => {
      document.removeEventListener('pointerdown', onPointerDown, true);
      window.removeEventListener('resize', onResize);
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('keydown', onKeyDown);
    });
  });

  return (
    <span ref={anchorEl} class="library-view-anchor">
      <Show
        when={!props.iconOnly}
        fallback={
          <IconButton
            size="sm"
            icon="adjustments-horizontal"
            label="显示选项"
            pressed={open()}
            aria-expanded={open()}
            aria-haspopup="dialog"
            onClick={() => setOpen((prev) => !prev)}
          />
        }
      >
        <Button
          size="sm"
          variant="ghost"
          icon="adjustments-horizontal"
          class="library-view-trigger"
          aria-label="显示选项"
          aria-expanded={open()}
          aria-haspopup="dialog"
          onClick={() => setOpen((prev) => !prev)}
        >
          显示选项
          <Show when={customized()}>
            <StatusDot state="ok" label="已自定义" />
          </Show>
        </Button>
      </Show>
      <Show when={open()}>
        <Portal>
          <div
            ref={popoverEl}
            class="library-view-popover"
            role="dialog"
            aria-label="显示选项"
            style={{ left: `${coords().left}px`, top: `${coords().top}px` }}
            onKeyDown={(event) => {
              if (event.key === 'Escape') {
                event.preventDefault();
                event.stopPropagation();
                closePopover(true);
              }
            }}
          >
            <Show when={props.includeSort}>
              <div class="library-view-section">
                <span class="library-view-heading">排序方式</span>
                <div class="library-view-sort">
                  <Select
                    size="sm"
                    aria-label="排序字段"
                    value={props.sort}
                    title={sortSelectTitle(props.sort)}
                    onChange={(value) =>
                      props.onSort(value as NonNullable<AssetListOptions['sort_by']>)
                    }
                    options={SORT_OPTIONS}
                  />
                  <IconButton
                    size="sm"
                    icon={props.direction === 'desc' ? 'sort-desc' : 'sort-asc'}
                    label={
                      props.direction === 'desc'
                        ? '排序方向：降序，切换为升序'
                        : '排序方向：升序，切换为降序'
                    }
                    onClick={props.onToggleDirection}
                  />
                </div>
              </div>
            </Show>

            <div class="library-view-section">
              <div class="library-view-slider-head">
                <span class="library-view-heading">缩略图大小</span>
                <span class="library-view-meta">{props.thumbSize} px</span>
              </div>
              <Slider
                label="缩略图大小"
                min={150}
                max={320}
                step={10}
                value={props.thumbSize}
                onInput={props.onThumbSize}
              />
            </div>

            <div class="library-view-section">
              <Show when={props.sort === 'captured_at'}>
                <div class="library-view-row">
                  <div class="library-view-row-copy">
                    <span>折叠连拍</span>
                    <small>同机身相邻 ≤ 2 秒自动归组</small>
                  </div>
                  <Switch
                    label="折叠连拍"
                    checked={props.burstStacks}
                    onChange={props.onBurstStacks}
                  />
                </div>
              </Show>
              <div class="library-view-row">
                <div class="library-view-row-copy">
                  <span>隐藏淘汰</span>
                  <small>
                    {props.hideRejectedLocked
                      ? '当前旗标筛选已排除淘汰照片'
                      : '网格中不显示已标记淘汰的照片'}
                  </small>
                </div>
                <Switch
                  label="隐藏淘汰"
                  checked={props.hideRejected}
                  disabled={props.hideRejectedLocked}
                  onChange={props.onHideRejected}
                />
              </div>
            </div>
          </div>
        </Portal>
      </Show>
    </span>
  );
}
