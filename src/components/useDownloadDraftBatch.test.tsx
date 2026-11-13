import { act, renderHook, waitFor } from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import { ParseDownloadResult, ScanDownloadProgress, ScanDownloadResult } from '../lib/types';
import { selectedResourceIds, useDownloadDraftBatch } from './useDownloadDraftBatch';
import { videoConfirmation } from '../lib/webVideo';

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}

function ready(taskId: string): ParseDownloadResult {
  return { confirmation: { taskId, title: taskId, requiresConfirmation: true, resources: [], processing: [] } };
}

it('probes an unmatched webpage after final scan classification instead of preparing HTML', async () => {
  const input = 'https://example.test/watch.shtml';
  const finalScan = deferred<ScanDownloadResult[]>();
  const prepare = vi.fn().mockRejectedValue(new Error('No video found'));
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: (_input, onProgress) => {
      onProgress?.({ entries: [{ input, valid: true, kind: 'direct' }], pending: [input] });
      return finalScan.promise;
    },
    prepare, discard: vi.fn().mockResolvedValue(undefined),
  }));
  let scan!: Promise<void>;
  await act(async () => { scan = result.current.start(input); });
  expect(prepare).not.toHaveBeenCalled();
  await act(async () => {
    finalScan.resolve([{ input, valid: true, kind: 'direct', canProbeVideo: true }]);
    await scan;
  });
  await waitFor(() => expect(result.current.items[0].status).toBe('failed'));
  expect(prepare).toHaveBeenCalledOnce();
  expect(prepare).toHaveBeenCalledWith(input, undefined, true);
  expect(result.current.items[0]).toMatchObject({ probeVideo: true, selected: false, error: 'No video found' });
  expect(result.current.items[0].result).toBeUndefined();
  expect(result.current.canSubmit).toBe(false);
});

it('keeps non-probe file, media, local-network and share inputs on their ordinary paths', async () => {
  const entries: ScanDownloadResult[] = [
    { input: 'https://example.test/archive.zip', valid: true, kind: 'direct', canProbeVideo: false },
    { input: 'https://example.test/movie.mp4', valid: true, kind: 'direct', canProbeVideo: false },
    { input: 'http://192.168.1.10/files', valid: true, kind: 'direct', canProbeVideo: false },
    { input: 'https://pan.baidu.com/s/example', valid: true, kind: 'cloud_share', canProbeVideo: false },
  ];
  const prepare = vi.fn(async (input: string) => ready(input));
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => entries, prepare, discard: vi.fn().mockResolvedValue(undefined),
  }));
  await act(async () => { await result.current.start('mixed inputs'); });
  await waitFor(() => expect(result.current.items.every(item => item.status === 'ready')).toBe(true));
  expect(prepare.mock.calls).toEqual(entries.map(entry => [entry.input, undefined]));
});

it('prepares at most four items concurrently and publishes fast results before slow ones', async () => {
  const entries: ScanDownloadResult[] = Array.from({ length: 5 }, (_, index) => ({
    input: `https://example.test/${index + 1}.bin`, valid: true as const, kind: 'direct' as const
  }));
  const work = entries.map(() => deferred<ParseDownloadResult>());
  let active = 0;
  let maxActive = 0;
  const prepare = vi.fn((input: string) => {
    const index = entries.findIndex(entry => entry.input === input);
    active += 1;
    maxActive = Math.max(maxActive, active);
    return work[index].promise.finally(() => { active -= 1; });
  });
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => entries,
    prepare,
    discard: vi.fn()
  }));

  await act(async () => { await result.current.start('five links'); });
  expect(result.current.items).toHaveLength(5);
  expect(result.current.items.every(item => item.status === 'parsing')).toBe(true);
  expect(prepare).toHaveBeenCalledTimes(4);
  expect(maxActive).toBe(4);

  await act(async () => { work[1].resolve(ready('task-2')); await work[1].promise; });
  await waitFor(() => expect(prepare).toHaveBeenCalledTimes(5));
  expect(result.current.items[0].status).toBe('parsing');
  expect(result.current.items[1].status).toBe('ready');
  await act(async () => {
    work[2].resolve(ready('task-3'));
    work[3].resolve(ready('task-4'));
    await Promise.all(work.slice(2, 4).map(item => item.promise));
  });
  await waitFor(() => expect(prepare).toHaveBeenCalledTimes(5));
  expect(maxActive).toBe(4);
  expect(result.current.canSubmit).toBe(false);
  act(() => result.current.toggleItem(result.current.items[0].id));
  act(() => result.current.toggleItem(result.current.items[4].id));
  expect(result.current.canSubmit).toBe(true);
  expect(result.current.items.map(item => item.input)).toEqual(entries.map(entry => entry.input));
  await act(async () => {
    work[0].resolve(ready('task-1'));
    work[4].resolve(ready('task-5'));
    await Promise.all([work[0].promise, work[4].promise]);
  });
  expect(result.current.items.filter(item => item.selected).map(item => item.input)).toEqual(entries.slice(1, 4).map(entry => entry.input));
});

it('keeps access-code state on one item, retries it, and discards only unsubmitted references', async () => {
  const first = deferred<ParseDownloadResult>();
  const second = deferred<ParseDownloadResult>();
  const prepare = vi.fn()
    .mockReturnValueOnce(first.promise)
    .mockReturnValueOnce(second.promise);
  const discard = vi.fn().mockResolvedValue(undefined);
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => [{ input: 'https://pan.baidu.com/s/example', valid: true, kind: 'cloud_share' }],
    prepare,
    discard,
    concurrency: 4
  }));

  await act(async () => { await result.current.start('share'); });
  await act(async () => {
    first.resolve({ shareAccessCodeRequired: { provider: 'baidu', message: '请输入提取码' } });
    await first.promise;
  });
  await waitFor(() => expect(result.current.items[0].status).toBe('access_code_required'));
  act(() => result.current.setAccessCode(result.current.items[0].id, '88in'));
  act(() => result.current.retry(result.current.items[0].id));
  expect(prepare).toHaveBeenLastCalledWith('https://pan.baidu.com/s/example', '88in');

  await act(async () => { second.resolve(ready('cloud-share://baidu/plan/file')); await second.promise; });
  await waitFor(() => expect(result.current.items[0].status).toBe('ready'));
  await act(async () => { await result.current.discardAll(); });
  expect(discard).toHaveBeenCalledWith('cloud-share://baidu/plan/file');

  act(() => result.current.markSubmitted([result.current.items[0].id]));
  await act(async () => { await result.current.discardAll(); });
  expect(discard).toHaveBeenCalledTimes(1);
});

it('returns an access-code task to an empty code field when the entered code is rejected', async () => {
  const first = deferred<ParseDownloadResult>();
  const rejectedCode = deferred<ParseDownloadResult>();
  const acceptedCode = deferred<ParseDownloadResult>();
  const prepare = vi.fn()
    .mockReturnValueOnce(first.promise)
    .mockReturnValueOnce(rejectedCode.promise)
    .mockReturnValueOnce(acceptedCode.promise);
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => [{ input: 'https://pan.quark.cn/s/example', valid: true, kind: 'cloud_share' }],
    prepare,
    discard: vi.fn(),
  }));

  await act(async () => { await result.current.start('share'); });
  await act(async () => {
    first.resolve({ shareAccessCodeRequired: { provider: 'quark', message: '请输入分享码' } });
    await first.promise;
  });
  await waitFor(() => expect(result.current.items[0].status).toBe('access_code_required'));

  act(() => result.current.setAccessCode(result.current.items[0].id, 'wrong'));
  act(() => result.current.retry(result.current.items[0].id));
  await act(async () => {
    rejectedCode.reject(new Error('夸克分享访问码错误'));
    await rejectedCode.promise.catch(() => undefined);
  });

  await waitFor(() => expect(result.current.items[0]).toMatchObject({
    status: 'access_code_required',
    accessCode: '',
    error: '夸克分享访问码错误',
  }));

  act(() => result.current.setAccessCode(result.current.items[0].id, 'correct'));
  act(() => result.current.retry(result.current.items[0].id));
  await act(async () => {
    acceptedCode.resolve(ready('cloud-share://quark/plan/file'));
    await acceptedCode.promise;
  });
  await waitFor(() => expect(result.current.items[0].status).toBe('ready'));
});

it('discards a draft that finishes parsing after the batch window was closed', async () => {
  const work = deferred<ParseDownloadResult>();
  const discard = vi.fn().mockResolvedValue(undefined);
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => [{ input: 'https://example.test/slow.zip', valid: true, kind: 'direct' }],
    prepare: () => work.promise,
    discard,
  }));

  await act(async () => { await result.current.start('slow'); });
  await act(async () => { await result.current.discardAll(); });
  await act(async () => {
    work.resolve(ready('slow-draft'));
    await work.promise;
  });

  await waitFor(() => expect(discard).toHaveBeenCalledWith('slow-draft'));
});

function shareWithFiles(taskId: string): ParseDownloadResult {
  return {
    confirmation: {
      taskId,
      title: taskId,
      requiresConfirmation: true,
      resources: ['a.7z', 'b.7z', 'c.7z'].map((name, index) => ({ id: `r${index}`, name: `folder/${name}`, size: '1 MB', selected: true, index })),
      processing: [],
    },
  };
}

async function readyShareBatch() {
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => [{ input: 'https://pan.baidu.com/s/1share', valid: true as const, kind: 'cloud_share' as const }],
    prepare: async () => shareWithFiles('share'),
    discard: vi.fn(),
  }));
  await act(async () => { await result.current.start('share'); });
  await waitFor(() => expect(result.current.items[0].status).toBe('ready'));
  return result;
}

it('selecting a file inside an unselected share selects the share and allows submission', async () => {
  const result = await readyShareBatch();
  const id = result.current.items[0].id;
  act(() => result.current.toggleItem(id));
  expect(result.current.items[0].selected).toBe(false);
  expect(result.current.items[0].result?.confirmation?.resources.some(resource => resource.selected)).toBe(false);
  expect(result.current.canSubmit).toBe(false);

  act(() => result.current.toggleResource(id, 'r2'));
  expect(result.current.items[0].selected).toBe(true);
  expect(result.current.canSubmit).toBe(true);

  act(() => result.current.toggleResource(id, 'r2'));
  expect(result.current.items[0].selected).toBe(false);
  expect(result.current.canSubmit).toBe(false);
});

it('toggling a partially selected share selects every file, then clears them all', async () => {
  const result = await readyShareBatch();
  const id = result.current.items[0].id;
  act(() => result.current.toggleResource(id, 'r0'));
  act(() => result.current.toggleItem(id));
  expect(result.current.items[0].selected).toBe(true);
  expect(result.current.items[0].result?.confirmation?.resources.every(resource => resource.selected)).toBe(true);

  act(() => result.current.toggleItem(id));
  expect(result.current.items[0].selected).toBe(false);
  expect(result.current.items[0].result?.confirmation?.resources.every(resource => !resource.selected)).toBe(true);
});

it('does not prepare anything when a slow site scan finishes after closing the dialog', async () => {
  const pending = deferred<ScanDownloadResult[]>();
  const prepare = vi.fn();
  let progress!: (event: ScanDownloadProgress) => void;
  const entries: ScanDownloadResult[] = [{ input: 'https://example.test/page', kind: 'web_video', valid: true }];
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: (_input, onProgress) => {
      progress = onProgress!;
      progress({ entries, pending: [entries[0].input] });
      return pending.promise;
    }, prepare, discard: vi.fn(),
  }));
  let scan: Promise<void>;
  act(() => { scan = result.current.start('https://example.test/page'); });
  await act(async () => { await result.current.discardAll(); });
  await act(async () => {
    progress({ entries, pending: [] });
    pending.resolve(entries);
    await scan;
  });
  expect(prepare).not.toHaveBeenCalled();
});

it('discards removed and superseded results without publishing them into the next batch', async () => {
  const removed = deferred<ParseDownloadResult>();
  const superseded = deferred<ParseDownloadResult>();
  const current = deferred<ParseDownloadResult>();
  const discard = vi.fn().mockResolvedValue(undefined);
  const prepare = vi.fn()
    .mockReturnValueOnce(removed.promise)
    .mockReturnValueOnce(superseded.promise)
    .mockReturnValueOnce(current.promise);
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async input => input.split(' ').map(value => ({ input: value, valid: true, kind: 'direct' })),
    prepare, discard,
  }));
  await act(async () => { await result.current.start('removed old'); });
  act(() => result.current.remove(result.current.items[0].id));
  await act(async () => { await result.current.start('current'); });
  await act(async () => {
    current.resolve(ready('current-preview'));
    removed.resolve(ready('removed-preview'));
    superseded.resolve(ready('old-preview'));
    await Promise.all([current.promise, removed.promise, superseded.promise]);
  });
  expect(result.current.items.map(item => [item.input, item.status])).toEqual([['current', 'ready']]);
  expect(discard).toHaveBeenCalledWith('removed-preview');
  expect(discard).toHaveBeenCalledWith('old-preview');
  expect(discard).not.toHaveBeenCalledWith('current-preview');
});

it('ignores a replaced scan even if it completes after the newer scan', async () => {
  const slowScan = deferred<ScanDownloadResult[]>();
  const prepare = vi.fn().mockResolvedValue(ready('new-preview'));
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: input => input === 'old' ? slowScan.promise : Promise.resolve([{ input: 'new', valid: true, kind: 'direct' }]),
    prepare, discard: vi.fn().mockResolvedValue(undefined),
  }));
  let oldScan!: Promise<void>;
  act(() => { oldScan = result.current.start('old'); });
  await act(async () => { await result.current.start('new'); });
  await act(async () => {
    slowScan.resolve([{ input: 'old', valid: true, kind: 'direct' }]);
    await oldScan;
  });
  expect(prepare).toHaveBeenCalledTimes(1);
  expect(prepare).toHaveBeenCalledWith('new', undefined);
  expect(result.current.items[0].input).toBe('new');
});

it('releases unsubmitted previews on unmount and discards pending results when they arrive', async () => {
  const pending = deferred<ParseDownloadResult>();
  const discard = vi.fn().mockResolvedValue(undefined);
  const { result, unmount } = renderHook(() => useDownloadDraftBatch({
    scan: async () => ['submitted', 'ready', 'pending'].map(input => ({ input, valid: true, kind: 'direct' })),
    prepare: async input => input === 'pending' ? pending.promise : ready(input + '-preview'),
    discard,
  }));
  await act(async () => { await result.current.start('batch'); });
  await waitFor(() => expect(result.current.items[1].status).toBe('ready'));
  act(() => result.current.markSubmitted([result.current.items[0].id]));
  unmount();
  expect(discard).toHaveBeenCalledWith('ready-preview');
  expect(discard).not.toHaveBeenCalledWith('submitted-preview');
  await act(async () => { pending.resolve(ready('pending-preview')); await pending.promise; });
  expect(discard).toHaveBeenCalledWith('pending-preview');
});

it('keeps a deselected video and its chosen resolution unchanged when reparsing it', async () => {
  let version = 0;
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => [{ input: 'https://example.test/video', valid: true, kind: 'web_video' }],
    prepare: async () => {
      version += 1;
      return { confirmation: videoConfirmation({
        previewId: `preview-${version}`, pageUrl: 'https://example.test/video', title: 'Video', site: 'Example', duration: 10, thumbnail: null,
        formats: [
          { id: `large-${version}`, width: 1280, height: 720, ext: 'mp4', bytes: null, estimated: false },
          { id: `small-${version}`, width: 640, height: 360, ext: 'mp4', bytes: null, estimated: false },
        ],
      }) };
    },
    discard: vi.fn().mockResolvedValue(undefined),
  }));
  await act(async () => { await result.current.start('video'); });
  await waitFor(() => expect(result.current.items[0].status).toBe('ready'));
  const id = result.current.items[0].id;
  act(() => result.current.toggleResource(id, 'small-1'));
  act(() => result.current.toggleResource(id, 'large-1'));
  act(() => result.current.toggleItem(id));
  act(() => result.current.retry(id));
  await waitFor(() => expect(result.current.items[0].status).toBe('ready'));
  expect(result.current.items[0].selected).toBe(false);
  expect(selectedResourceIds(result.current.items[0])).toEqual(['small-2']);
  expect(result.current.canSubmit).toBe(false);
});

it('does not start a replacement extraction if its draft is removed while discarding the old preview', async () => {
  const release = deferred<void>();
  const prepare = vi.fn().mockResolvedValue(ready('old-preview'));
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: async () => [{ input: 'https://example.test/video', valid: true, kind: 'web_video' }],
    prepare, discard: () => release.promise,
  }));
  await act(async () => { await result.current.start('video'); });
  await waitFor(() => expect(result.current.items[0].status).toBe('ready'));
  const id = result.current.items[0].id;
  act(() => result.current.retry(id));
  act(() => result.current.remove(id));
  await act(async () => { release.resolve(); await release.promise; });
  expect(prepare).toHaveBeenCalledTimes(1);
  expect(result.current.items).toHaveLength(0);
});

it('publishes early scan entries and merges final routes without replaying work or restoring removed choices', async () => {
  const entries: ScanDownloadResult[] = ['known', 'pending', 'removed'].map((name, index) => ({
    input: `https://example.test/${name}`, valid: true, kind: index ? 'direct' : 'web_video',
  }));
  const finalScan = deferred<ScanDownloadResult[]>();
  let progress!: (event: ScanDownloadProgress) => void;
  const prepare = vi.fn(async input => ready(input));
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: (_input, onProgress) => {
      progress = onProgress!;
      progress({ entries, pending: entries.slice(1).map(entry => entry.input) });
      return finalScan.promise;
    },
    prepare, discard: vi.fn().mockResolvedValue(undefined),
  }));
  let scan!: Promise<void>;
  await act(async () => { scan = result.current.start('batch'); });
  expect(result.current.items).toHaveLength(3);
  expect(prepare).toHaveBeenCalledTimes(1);
  expect(prepare).toHaveBeenCalledWith(entries[0].input, undefined, true);
  expect(result.current.items[0].status).toBe('ready');
  act(() => result.current.retry(result.current.items[1].id));
  expect(prepare).toHaveBeenCalledTimes(1);
  act(() => result.current.toggleItem(result.current.items[0].id));
  act(() => result.current.toggleItem(result.current.items[1].id));
  act(() => result.current.remove(result.current.items[2].id));
  const matched: ScanDownloadResult[] = entries.map(entry => ({ input: entry.input, valid: true, kind: 'web_video' }));
  await act(async () => {
    progress({ entries: matched, pending: [] });
    finalScan.resolve(matched);
    await scan;
  });
  expect(prepare).toHaveBeenCalledTimes(2);
  expect(prepare).toHaveBeenLastCalledWith(entries[1].input, undefined, true);
  expect(result.current.items.map(item => [item.input, item.selected, item.status])).toEqual([
    [entries[0].input, false, 'ready'], [entries[1].input, false, 'ready'],
  ]);
  act(() => progress({ entries, pending: entries.slice(1).map(entry => entry.input) }));
  expect(prepare).toHaveBeenCalledTimes(2);
  expect(result.current.items.every(item => item.status === 'ready')).toBe(true);
});

it('keeps completed early parses when the remaining scan fails and ignores late scan events', async () => {
  const finalScan = deferred<ScanDownloadResult[]>();
  let progress!: (event: ScanDownloadProgress) => void;
  const entries: ScanDownloadResult[] = ['known', 'pending'].map(input => ({ input, valid: true, kind: 'web_video' }));
  const prepare = vi.fn(async input => ready(input));
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: (_input, onProgress) => {
      progress = onProgress!;
      progress({ entries, pending: ['pending'] });
      return finalScan.promise;
    },
    prepare, discard: vi.fn().mockResolvedValue(undefined),
  }));
  let scan!: Promise<void>;
  await act(async () => { scan = result.current.start('batch'); });
  await act(async () => {
    finalScan.reject(new Error('Scan failed'));
    await scan;
  });
  expect(result.current.items[0].status).toBe('ready');
  expect(result.current.items[1]).toMatchObject({ status: 'failed', error: 'Scan failed' });
  act(() => progress({ entries, pending: [] }));
  expect(prepare).toHaveBeenCalledTimes(1);
});

it.each([false, true])('keeps repeated URLs as independent scan rows (remove first: %s)', async removeFirst => {
  const url = 'https://example.test/repeated';
  const entries: ScanDownloadResult[] = Array.from({ length: 2 }, () => ({ input: url, valid: true, kind: 'direct' }));
  const matched: ScanDownloadResult[] = entries.map(entry => ({ input: entry.input, valid: true, kind: 'web_video' }));
  const finalScan = deferred<ScanDownloadResult[]>();
  let progress!: (event: ScanDownloadProgress) => void;
  let parsed = 0;
  const prepare = vi.fn(async () => ready(`preview-${++parsed}`));
  const { result } = renderHook(() => useDownloadDraftBatch({
    scan: (_input, onProgress) => {
      progress = onProgress!;
      progress({ entries, pending: [url] });
      return finalScan.promise;
    },
    prepare, discard: vi.fn().mockResolvedValue(undefined),
  }));
  let scan!: Promise<void>;
  await act(async () => { scan = result.current.start(`${url}\n${url}`); });
  const originalIds = result.current.items.map(item => item.id);
  expect(originalIds).toHaveLength(2);
  expect(new Set(originalIds).size).toBe(2);
  expect(prepare).not.toHaveBeenCalled();
  act(() => result.current.toggleItem(originalIds[1]));
  if (removeFirst) act(() => result.current.remove(originalIds[0]));

  await act(async () => { progress({ entries: matched, pending: [] }); });
  await waitFor(() => expect(prepare).toHaveBeenCalledTimes(removeFirst ? 1 : 2));
  await act(async () => { finalScan.resolve(matched); await scan; });
  expect(prepare).toHaveBeenCalledTimes(removeFirst ? 1 : 2);
  expect(result.current.items.map(item => item.id)).toEqual(removeFirst ? [originalIds[1]] : originalIds);
  expect(result.current.items.every(item => item.status === 'ready')).toBe(true);
  expect(result.current.items.find(item => item.id === originalIds[1])?.selected).toBe(false);
  expect(new Set(result.current.items.map(item => item.result?.confirmation?.taskId)).size).toBe(removeFirst ? 1 : 2);
});
