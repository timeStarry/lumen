import { describe, expect, it } from 'vitest';
import {
  DEFAULT_SORT_BY_KEY,
  DEFAULT_SORT_DIRECTION_KEY,
  INITIAL_LIBRARY_SORT,
  readDefaultLibrarySort,
  writeDefaultLibrarySort,
} from '../src/sort-preferences';

function createMemoryStorage(seed: Record<string, string> = {}) {
  const map = new Map<string, string>(Object.entries(seed));
  return {
    getItem(key: string) {
      return map.get(key) ?? null;
    },
    setItem(key: string, value: string) {
      map.set(key, value);
    },
  };
}

describe('sort-preferences', () => {
  it('defaults to captured_at desc when nothing is stored', () => {
    const store = createMemoryStorage();
    expect(readDefaultLibrarySort(store)).toEqual({
      sortBy: 'captured_at',
      direction: 'desc',
    });
    expect(INITIAL_LIBRARY_SORT).toEqual({
      sortBy: 'captured_at',
      direction: 'desc',
    });
  });

  it('persists partial updates without clobbering the sibling field', () => {
    const store = createMemoryStorage();
    expect(writeDefaultLibrarySort({ sortBy: 'filename' }, store)).toEqual({
      sortBy: 'filename',
      direction: 'desc',
    });
    expect(writeDefaultLibrarySort({ direction: 'asc' }, store)).toEqual({
      sortBy: 'filename',
      direction: 'asc',
    });
    expect(readDefaultLibrarySort(store)).toEqual({
      sortBy: 'filename',
      direction: 'asc',
    });
  });

  it('falls back safely when storage holds corrupted values or throws', () => {
    const corrupted = createMemoryStorage({
      [DEFAULT_SORT_BY_KEY]: 'invalid_field',
      [DEFAULT_SORT_DIRECTION_KEY]: 'sideways',
    });
    expect(readDefaultLibrarySort(corrupted)).toEqual({
      sortBy: 'captured_at',
      direction: 'desc',
    });

    const throwing = {
      getItem(): string | null {
        throw new Error('storage denied');
      },
      setItem(): void {
        throw new Error('storage denied');
      },
    };
    expect(readDefaultLibrarySort(throwing)).toEqual({
      sortBy: 'captured_at',
      direction: 'desc',
    });
    expect(writeDefaultLibrarySort({ sortBy: 'rating', direction: 'asc' }, throwing)).toEqual({
      sortBy: 'rating',
      direction: 'asc',
    });
  });
});
