import { describe, expect, it } from 'vitest';
import { inspectorMetadataGroups } from '../src/inspector-metadata';
import { formatDurationSeconds, isVideoExtension, type Asset } from '../src/model';
import { browserAssetList } from '../src/workspace';

const sampleAsset = (overrides: Partial<Asset>): Asset => ({
  id: 'a-1',
  library_id: 'lib-1',
  path: 'clip.mp4',
  filename: 'clip.mp4',
  extension: 'mp4',
  kind: 'video',
  size: 1024,
  modified_at: '2026-11-15T10:00:00Z',
  rating: 0,
  metadata: {},
  availability: 'available',
  ...overrides,
});

describe('video format support helpers', () => {
  it('recognizes camera and web video extensions case-insensitively', () => {
    for (const ext of ['mp4', '.MOV', 'M4V', 'webm', 'avi', 'mkv', 'MTS', 'm2ts', '3gp']) {
      expect(isVideoExtension(ext)).toBe(true);
    }
    for (const ext of ['jpg', 'cr3', 'nef', 'mp3', 'wav', 'txt']) {
      expect(isVideoExtension(ext)).toBe(false);
    }
  });

  it('formats clip durations cleanly', () => {
    expect(formatDurationSeconds(0)).toBe('0:00');
    expect(formatDurationSeconds(9.4)).toBe('0:09');
    expect(formatDurationSeconds(75)).toBe('1:15');
    expect(formatDurationSeconds(3661)).toBe('1:01:01');
  });

  it('filters photo-only vs video-only assets in browserAssetList', () => {
    const items: Asset[] = [
      sampleAsset({ id: '1', filename: 'a.jpg', extension: 'jpg', kind: 'raster' }),
      sampleAsset({ id: '2', filename: 'b.nef', extension: 'nef', kind: 'raw' }),
      sampleAsset({ id: '3', filename: 'c.mp4', extension: 'mp4', kind: 'video' }),
    ];

    expect(browserAssetList(items, { format: 'video' }).items.map((x) => x.id)).toEqual(['3']);
    expect(browserAssetList(items, { format: 'photo' }).items.map((x) => x.id).sort()).toEqual([
      '1',
      '2',
    ]);
    expect(browserAssetList(items, { format: 'all' }).total).toBe(3);
  });

  it('surfaces Duration, Video Codec, and Frame Rate under the 视频 inspector group', () => {
    const groups = inspectorMetadataGroups({
      Format: 'MP4',
      Duration: '0:42',
      'Video Codec': 'HEVC (H.265)',
      'Frame Rate': '59.94 fps',
      Camera: 'Nikon Z30',
    });
    const videoGroup = groups.find((g) => g.title === '视频');
    expect(videoGroup).toBeDefined();
    expect(videoGroup?.parameters).toEqual([
      { label: '时长', value: '0:42' },
      { label: '视频编码', value: 'HEVC (H.265)' },
      { label: '帧率', value: '59.94 fps' },
    ]);
  });
});
