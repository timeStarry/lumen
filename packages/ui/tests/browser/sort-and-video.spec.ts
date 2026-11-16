import { expect, test } from '@playwright/test';
import { selectChoice } from './fixtures';

test('default library sort preference and video format filtering & stage', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));

  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto('/');

  // 1. Configure default library sort in Settings -> 外观与浏览 to Filename · Ascending.
  await page
    .getByRole('navigation', { name: '主导航' })
    .getByRole('button', { name: '设置', exact: true })
    .click();

  await selectChoice(page.locator('#library-default-sort'), 'filename');
  await selectChoice(page.getByLabel('默认排序方向'), 'asc');

  // Navigate back to Library; entering the library applies the configured default sort.
  await page
    .getByRole('navigation', { name: '主导航' })
    .getByRole('button', { name: '图片库', exact: true })
    .click();

  const { pngBase64, webmBase64 } = await page.evaluate(async () => {
    const canvas = document.createElement('canvas');
    canvas.width = 320;
    canvas.height = 240;
    const ctx = canvas.getContext('2d')!;
    ctx.fillStyle = '#4c78a8';
    ctx.fillRect(0, 0, 320, 240);
    const png = canvas.toDataURL('image/png').split(',')[1];

    const stream = canvas.captureStream(15);
    const recorder = new MediaRecorder(stream, { mimeType: 'video/webm' });
    const chunks: BlobPart[] = [];
    recorder.ondataavailable = (e) => {
      if (e.data.size > 0) chunks.push(e.data);
    };
    const stopped = new Promise<void>((resolve) => {
      recorder.onstop = () => resolve();
    });
    recorder.start();
    ctx.fillStyle = '#e07a5f';
    ctx.fillRect(40, 40, 240, 160);
    await new Promise((r) => setTimeout(r, 120));
    recorder.stop();
    stream.getTracks().forEach((t) => t.stop());
    await stopped;

    const blob = new Blob(chunks, { type: 'video/webm' });
    const buf = new Uint8Array(await blob.arrayBuffer());
    let binary = '';
    for (let i = 0; i < buf.length; i++) binary += String.fromCharCode(buf[i]);
    return { pngBase64: png, webmBase64: btoa(binary) };
  });

  // Load two photos (`zulu.png`, `alpha.png`) plus one playable `.webm` video clip.
  await page.getByLabel('选择浏览器演示图片').setInputFiles([
    {
      name: 'zulu.png',
      mimeType: 'image/png',
      buffer: Buffer.from(pngBase64, 'base64'),
    },
    {
      name: 'alpha.png',
      mimeType: 'image/png',
      buffer: Buffer.from(pngBase64, 'base64'),
    },
    {
      name: 'clip-mid.webm',
      mimeType: 'video/webm',
      buffer: Buffer.from(webmBase64, 'base64'),
    },
  ]);

  const tiles = page.locator('button.thumbnail.tile');
  await expect(tiles).toHaveCount(3);
  // Alphabetical ascending: alpha.png -> clip-mid.webm -> zulu.png
  await expect(tiles.nth(0)).toHaveAttribute('aria-label', /alpha\.png/);
  await expect(tiles.nth(1)).toHaveAttribute('aria-label', /clip-mid\.webm/);
  await expect(tiles.nth(2)).toHaveAttribute('aria-label', /zulu\.png/);

  // Video tile displays the video badge.
  await expect(tiles.nth(1).locator('.video-badge')).toBeVisible();

  // Temporarily flip toolbar sort direction to Descending, then verify Reset View restores Filename · Asc.
  await page.getByRole('button', { name: /排序方向：升序/ }).click();
  await expect(tiles.nth(0)).toHaveAttribute('aria-label', /zulu\.png/);

  // Narrow viewport so `显示选项` popover appears, then click `重置视图`.
  await page.setViewportSize({ width: 980, height: 800 });
  await page.getByRole('button', { name: /显示选项/ }).click();
  await page.getByRole('button', { name: '重置视图' }).click();
  await page.setViewportSize({ width: 1440, height: 900 });
  await expect(tiles.nth(0)).toHaveAttribute('aria-label', /alpha\.png/);

  // 2. Filter by Video (`视频`) and Photo-only (`仅照片`).
  await page.locator('button.filter-toggle').click();
  const filterGroup = page.getByRole('group', { name: '筛选条件' });
  await expect(filterGroup).toBeVisible();

  await selectChoice(filterGroup.getByLabel('文件格式筛选'), 'video');
  await expect(tiles).toHaveCount(1);
  await expect(tiles.nth(0)).toHaveAttribute('aria-label', /clip-mid\.webm/);

  await selectChoice(filterGroup.getByLabel('文件格式筛选'), 'photo');
  await expect(tiles).toHaveCount(2);
  await expect(page.getByRole('button', { name: /clip-mid\.webm/ })).toHaveCount(0);

  await filterGroup.getByRole('button', { name: '清除全部' }).click();
  await expect(tiles).toHaveCount(3);

  // 3. Open photo first, then step right (`→`) to video: verify custom darkroom controls & zero corner jump.
  await page.getByRole('button', { name: /alpha\.png/ }).dblclick();
  await expect(page.locator('.viewer-overlay .photo-stage')).toBeVisible();
  const photoTlTop = await page
    .locator('.viewer-overlay .nl-overlay-corner--tl')
    .evaluate((el) => parseFloat(getComputedStyle(el).top));

  await page.keyboard.press('ArrowRight');
  await expect(page.locator('.viewer-overlay .video-stage')).toBeVisible();
  await expect(page.locator('.viewer-overlay .nl-widget-hist')).toHaveCount(0);

  const videoEl = page.locator('.viewer-overlay video.video-stage-player');
  await expect(videoEl).toBeVisible();
  await expect(videoEl).not.toHaveAttribute('controls');

  const videoBar = page.locator('.viewer-overlay').getByRole('region', { name: '视频播放控制' });
  await expect(videoBar).toBeVisible();
  await expect(videoBar.getByRole('button', { name: /播放倍速：当前 1 倍/ })).toBeVisible();
  await videoBar.getByRole('button', { name: /播放倍速：当前 1 倍/ }).click();
  await expect(videoBar.getByRole('button', { name: /播放倍速：当前 1\.5 倍/ })).toBeVisible();

  await page.keyboard.press('m');
  await expect(videoBar.getByRole('button', { name: /取消静音（M）/ })).toBeVisible();

  const videoTlTop = await page
    .locator('.viewer-overlay .nl-overlay-corner--tl')
    .evaluate((el) => parseFloat(getComputedStyle(el).top));
  expect(videoTlTop).toBe(photoTlTop);

  const barBox = await page.locator('.viewer-overlay .nl-viewer-bar').boundingBox();
  const stagePaddingTop = await page
    .locator('.viewer-overlay .video-stage')
    .evaluate((el) => parseFloat(getComputedStyle(el).paddingTop));
  expect(barBox).not.toBeNull();
  expect(stagePaddingTop).toBeGreaterThanOrEqual(barBox!.height);

  await page.keyboard.press('p');
  await page.keyboard.press('Escape');
  await expect(page.getByRole('button', { name: /clip-mid\.webm.*留用/ })).toBeVisible();

  expect(errors).toEqual([]);
});
