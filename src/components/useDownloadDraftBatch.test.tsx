import { act, renderHook, waitFor } from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import { ParseDownloadResult, ScanDownloadResult } from '../lib/types';
import { useDownloadDraftBatch } from './useDownloadDraftBatch';

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}

function ready(taskId: string): ParseDownloadResult {
  return { confirmation: { taskId, title: taskId, requiresConfirmation: true, resources: [], processing: [] } };
}

it('prepares at most four items concurrently and deselecting a pending item unblocks submission', async () => {
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
    discard: vi.fn(),
    concurrency: 4
  }));

  await act(async () => { await result.current.start('five links'); });
  expect(result.current.items).toHaveLength(5);
  expect(result.current.items.every(item => item.status === 'parsing')).toBe(true);
  expect(prepare).toHaveBeenCalledTimes(4);
  expect(maxActive).toBe(4);

  await act(async () => { work[0].resolve(ready('task-1')); await work[0].promise; });
  await waitFor(() => expect(prepare).toHaveBeenCalledTimes(5));
  await act(async () => {
    work[1].resolve(ready('task-2'));
    work[2].resolve(ready('task-3'));
    work[3].resolve(ready('task-4'));
    await Promise.all(work.slice(1, 4).map(item => item.promise));
  });
  expect(result.current.canSubmit).toBe(false);
  act(() => result.current.toggleItem(result.current.items[4].id));
  expect(result.current.canSubmit).toBe(true);
  expect(result.current.items.map(item => item.input)).toEqual(entries.map(entry => entry.input));
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
  const { result } = renderHook(() => useDownloadDraftBatch({scan: () => pending.promise, prepare, discard:vi.fn()}));
  let scan: Promise<void>;
  act(() => { scan = result.current.start('https://example.test/page'); });
  await act(async () => { await result.current.discardAll(); });
  await act(async () => {
    pending.resolve([{input:'https://example.test/page',kind:'web_video',valid:true}]);
    await scan;
  });
  expect(prepare).not.toHaveBeenCalled();
});
