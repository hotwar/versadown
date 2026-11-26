import { beforeEach, expect, it, vi } from 'vitest';

const native = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }));

import { browserShareCaptureMessage, captureSignalKind, openBrowserShare, reopenBrowserShare } from './browserShare';

beforeEach(() => native.invoke.mockReset());

it.each([false, true])('opens a share with the selected directory and privacy (%s)', async (privateDownload) => {
  native.invoke.mockResolvedValue(undefined);
  await openBrowserShare('gofile', 'https://gofile.io/d/AbC123', 'D:/Selected', privateDownload);
  expect(native.invoke).toHaveBeenCalledWith('cloud_open_browser_share', {
    url: 'https://gofile.io/d/AbC123',
    destinationDirectory: 'D:/Selected',
    privateDownload,
    title: expect.stringContaining('Gofile'),
    failureNotice: expect.stringContaining('没有被软件接管'),
  });
});

it('reopens the share page of a task, taking the provider from its stable source', async () => {
  native.invoke.mockResolvedValue(undefined);
  const source = `cloud-share://gofile/${'a'.repeat(64)}/${'b'.repeat(64)}`;
  await reopenBrowserShare('task-1', source);
  expect(native.invoke).toHaveBeenCalledWith('cloud_reopen_browser_share', { taskId: 'task-1', title: expect.stringContaining('Gofile'), failureNotice: expect.stringContaining('没有被软件接管') });
  await expect(reopenBrowserShare('task-2', 'https://example.test/file')).rejects.toThrow();
  expect(native.invoke).toHaveBeenCalledTimes(1);
});

it('words each capture outcome, translating backend failure keys', () => {
  expect(browserShareCaptureMessage({ provider: 'gofile', sharePage: 'https://gofile.io/d/AbC123', outcome: 'queued', name: 'a.zip' }))
    .toEqual({ kind: 'success', message: expect.stringContaining('已加入下载：a.zip') });
  expect(browserShareCaptureMessage({ provider: 'gofile', sharePage: 'https://gofile.io/d/AbC123', outcome: 'duplicate', name: 'a.zip' }).message).toContain('已在下载列表中');
  expect(browserShareCaptureMessage({ provider: 'gofile', sharePage: 'https://gofile.io/d/AbC123', outcome: 'failed', name: '', message: 'cloud.error.gofile.linkExpired' }))
    .toEqual({ kind: 'error', message: 'Gofile 下载链接已过期，请重新打开分享页' });
  expect(browserShareCaptureMessage({ provider: 'gofile', sharePage: 'https://gofile.io/d/AbC123', outcome: 'failed', name: '', message: '网盘请求失败' }))
    .toEqual({ kind: 'error', message: '网盘请求失败' });
});

it('only a taken-over file finishes the share draft; a failed capture leaves it', () => {
  const base = { provider: 'gofile' as const, sharePage: 'https://gofile.io/d/AbC123', name: 'a.zip' };
  expect(captureSignalKind({ ...base, outcome: 'queued' })).toBe('finished');
  expect(captureSignalKind({ ...base, outcome: 'duplicate' })).toBe('finished');
  expect(captureSignalKind({ ...base, outcome: 'failed' })).toBeNull();
});
