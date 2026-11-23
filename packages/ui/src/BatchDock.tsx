import { For, Show, createSignal } from 'solid-js';
import Icon from './Icon';
import type { ColorLabel, Flag } from './model';
import { COLOR_LABEL_NAMES } from './undo';
import { Button, FloatingDock, IconButton } from './ui';
import './BatchDock.css';

const BATCH_COLORS: Array<Exclude<ColorLabel, 'none'>> = ['red', 'yellow', 'green', 'blue'];

/**
 * Floating glass batch-action dock shown when 2+ photos are selected.
 * Replaces the old inline top toolbar so multi-selection never shifts the grid vertically.
 */
export default function BatchDock(props: {
  count: number;
  sharedRating?: number;
  canTrash: boolean;
  exportBusy?: boolean;
  trashBusy?: boolean;
  onMark: (mark: { rating?: number; flag?: Flag; color_label?: ColorLabel }) => void;
  onCollect?: () => void;
  onRemoveFromCollection?: () => void;
  onExport: () => void;
  onExportPreset?: () => void;
  onTrash: () => void;
  onClear: () => void;
}) {
  const [hoverRating, setHoverRating] = createSignal(0);
  return (
    <Show when={props.count > 1}>
      <FloatingDock label="批量操作" class="library-batch batch-dock">
        <b class="batch-dock-count">已选 {props.count.toLocaleString()} 张</b>
        <span class="batch-dock-sep" aria-hidden="true" />
        <span
          class="library-batch-group batch-star-group"
          role="group"
          aria-label="批量评级"
          onMouseLeave={() => setHoverRating(0)}
        >
          <For each={[1, 2, 3, 4, 5]}>
            {(value) => (
              <button
                type="button"
                class="batch-star-btn"
                classList={{
                  filled: hoverRating()
                    ? value <= hoverRating()
                    : (props.sharedRating ?? 0) >= value,
                }}
                aria-label={`将所选照片评为 ${value} 星`}
                title={`${value} 星`}
                onMouseEnter={() => setHoverRating(value)}
                onFocus={() => setHoverRating(value)}
                onBlur={() => setHoverRating(0)}
                onClick={() => props.onMark({ rating: value })}
              >
                <Icon name="star" size={15} />
              </button>
            )}
          </For>
          <Button
            size="sm"
            variant="ghost"
            class="batch-compact-btn"
            aria-label="清除所选照片评级"
            title="清除评级"
            onClick={() => props.onMark({ rating: 0 })}
          >
            无星
          </Button>
        </span>
        <span class="batch-dock-sep" aria-hidden="true" />
        <span class="library-batch-group" role="group" aria-label="批量旗标">
          <Button
            size="sm"
            variant="ghost"
            class="batch-compact-btn"
            onClick={() => props.onMark({ flag: 'pick' })}
          >
            留用
          </Button>
          <Button
            size="sm"
            variant="ghost"
            class="batch-compact-btn"
            onClick={() => props.onMark({ flag: 'reject' })}
          >
            淘汰
          </Button>
          <Button
            size="sm"
            variant="ghost"
            class="batch-compact-btn"
            onClick={() => props.onMark({ flag: 'none' })}
          >
            清除旗标
          </Button>
        </span>
        <span class="batch-dock-sep" aria-hidden="true" />
        <span class="library-batch-group" role="group" aria-label="批量色标">
          <For each={BATCH_COLORS}>
            {(color) => (
              <button
                type="button"
                class="batch-color-btn"
                aria-label={`将所选照片设为${COLOR_LABEL_NAMES[color]}`}
                title={COLOR_LABEL_NAMES[color]}
                onClick={() => props.onMark({ color_label: color })}
              >
                <span class="batch-color-swatch" data-color={color} />
              </button>
            )}
          </For>
          <Button
            size="sm"
            variant="ghost"
            class="batch-compact-btn"
            aria-label="清除所选照片色标"
            onClick={() => props.onMark({ color_label: 'none' })}
          >
            无色
          </Button>
        </span>
        <Show when={props.onCollect || props.onRemoveFromCollection}>
          <span class="batch-dock-sep" aria-hidden="true" />
          <Show when={props.onCollect}>
            <Button
              size="sm"
              variant="ghost"
              class="batch-compact-btn"
              icon="folder-plus"
              onClick={() => props.onCollect!()}
            >
              加入精选集…
            </Button>
          </Show>
          <Show when={props.onRemoveFromCollection}>
            <Button
              size="sm"
              variant="ghost"
              class="batch-compact-btn"
              onClick={() => props.onRemoveFromCollection!()}
            >
              移出精选集
            </Button>
          </Show>
        </Show>
        <span class="batch-dock-sep" aria-hidden="true" />
        <span class="library-batch-group batch-action-tail">
          <Button
            size="sm"
            class="batch-compact-btn"
            icon="download"
            disabled={props.exportBusy}
            onClick={() => props.onExport()}
          >
            导出原片…
          </Button>
          <Show when={props.onExportPreset}>
            <Button
              size="sm"
              variant="ghost"
              class="batch-compact-btn"
              icon="export"
              disabled={props.exportBusy}
              onClick={() => props.onExportPreset!()}
            >
              导出 JPEG…
            </Button>
          </Show>
          <Show when={props.canTrash}>
            <Button
              size="sm"
              variant="danger"
              class="batch-compact-btn"
              icon="trash"
              disabled={props.trashBusy}
              onClick={() => props.onTrash()}
            >
              移到废纸篓
            </Button>
          </Show>
          <IconButton size="sm" icon="close" label="取消选择" onClick={() => props.onClear()} />
        </span>
      </FloatingDock>
    </Show>
  );
}
