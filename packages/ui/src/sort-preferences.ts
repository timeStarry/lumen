import type { AssetListOptions } from './model';

export type LibrarySortField = NonNullable<AssetListOptions['sort_by']>;
export type LibrarySortDirection = NonNullable<AssetListOptions['sort_direction']>;

export interface LibrarySortPreference {
  sortBy: LibrarySortField;
  direction: LibrarySortDirection;
}

export const DEFAULT_SORT_BY_KEY = 'nebulume.library.default-sort-by';
export const DEFAULT_SORT_DIRECTION_KEY = 'nebulume.library.default-sort-direction';
export const DEFAULT_SORT_CHANGED_EVENT = 'nebulume:default-sort-changed';
export const DEFAULT_THUMB_SIZE = 210;

export const INITIAL_LIBRARY_SORT: Readonly<LibrarySortPreference> = {
  sortBy: 'captured_at',
  direction: 'desc',
};

export const SORT_FIELD_OPTIONS: { value: LibrarySortField; label: string }[] = [
  { value: 'captured_at', label: '拍摄时间' },
  { value: 'created_at', label: '文件创建时间' },
  { value: 'modified_at', label: '文件修改时间' },
  { value: 'filename', label: '文件名' },
  { value: 'rating', label: '评级' },
];

export const SORT_DIRECTION_OPTIONS: { value: LibrarySortDirection; label: string }[] = [
  { value: 'desc', label: '降序' },
  { value: 'asc', label: '升序' },
];

const VALID_SORT_FIELDS = new Set<string>(SORT_FIELD_OPTIONS.map((item) => item.value));

export function isLibrarySortField(value: unknown): value is LibrarySortField {
  return typeof value === 'string' && VALID_SORT_FIELDS.has(value);
}

export function isLibrarySortDirection(value: unknown): value is LibrarySortDirection {
  return value === 'asc' || value === 'desc';
}

export function readDefaultLibrarySort(
  storage?: Pick<Storage, 'getItem'> | null,
): LibrarySortPreference {
  try {
    const store =
      storage !== undefined
        ? storage
        : typeof localStorage !== 'undefined'
          ? localStorage
          : null;
    if (!store) return { ...INITIAL_LIBRARY_SORT };
    const rawSortBy = store.getItem(DEFAULT_SORT_BY_KEY);
    const rawDirection = store.getItem(DEFAULT_SORT_DIRECTION_KEY);
    return {
      sortBy: isLibrarySortField(rawSortBy) ? rawSortBy : INITIAL_LIBRARY_SORT.sortBy,
      direction: isLibrarySortDirection(rawDirection)
        ? rawDirection
        : INITIAL_LIBRARY_SORT.direction,
    };
  } catch {
    return { ...INITIAL_LIBRARY_SORT };
  }
}

export function writeDefaultLibrarySort(
  next: Partial<LibrarySortPreference>,
  storage?: Pick<Storage, 'getItem' | 'setItem'> | null,
): LibrarySortPreference {
  const current = readDefaultLibrarySort(storage);
  const resolved: LibrarySortPreference = {
    sortBy: isLibrarySortField(next.sortBy) ? next.sortBy : current.sortBy,
    direction: isLibrarySortDirection(next.direction) ? next.direction : current.direction,
  };
  try {
    const store =
      storage !== undefined
        ? storage
        : typeof localStorage !== 'undefined'
          ? localStorage
          : null;
    store?.setItem(DEFAULT_SORT_BY_KEY, resolved.sortBy);
    store?.setItem(DEFAULT_SORT_DIRECTION_KEY, resolved.direction);
  } catch {
    /* Preference remains effective via CustomEvent for this window session. */
  }
  if (storage === undefined && typeof window !== 'undefined') {
    window.dispatchEvent(new CustomEvent(DEFAULT_SORT_CHANGED_EVENT, { detail: resolved }));
  }
  return resolved;
}
