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

  const pngBase64 = await page.evaluate(() => {
    const canvas = document.createElement('canvas');
    canvas.width = 640;
    canvas.height = 480;
    const ctx = canvas.getContext('2d')!;
    ctx.fillStyle = '#4c78a8';
    ctx.fillRect(0, 0, 640, 480);
    return canvas.toDataURL('image/png').split(',')[1];
  });

  // Load two photos (`zulu.png`, `alpha.png`) plus one `.mp4` video clip.
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
      name: 'clip-mid.mp4',
      mimeType: 'video/mp4',
      buffer: Buffer.from('00000014667479706d703432000000006d703432', 'hex'),
    },
  ]);

  const tiles = page.locator('button.thumbnail.tile');
  await expect(tiles).toHaveCount(3);
  // Alphabetical ascending: alpha.png -> clip-mid.mp4 -> zulu.png
  await expect(tiles.nth(0)).toHaveAttribute('aria-label', /alpha\.png/);
  await expect(tiles.nth(1)).toHaveAttribute('aria-label', /clip-mid\.mp4/);
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
  await expect(tiles.nth(0)).toHaveAttribute('aria-label', /clip-mid\.mp4/);

  await selectChoice(filterGroup.getByLabel('文件格式筛选'), 'photo');
  await expect(tiles).toHaveCount(2);
  await expect(page.getByRole('button', { name: /clip-mid\.mp4/ })).toHaveCount(0);

  await filterGroup.getByRole('button', { name: '清除全部' }).click();
  await expect(tiles).toHaveCount(3);

  // 3. Double-click the video tile to open the inline video stage in Viewer overlay, and mark it (`P`).
  await page.getByRole('button', { name: /clip-mid\.mp4/ }).dblclick();
  await expect(page.locator('.viewer-overlay .video-stage')).toBeVisible();
  await page.keyboard.press('p');
  await page.keyboard.press('Escape');
  await expect(page.getByRole('button', { name: /clip-mid\.mp4.*留用/ })).toBeVisible();

  expect(errors).toEqual([]);
});
