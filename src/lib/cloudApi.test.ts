import { vi, expect, it } from 'vitest';
const native = { invoke: vi.fn().mockResolvedValue(undefined) };
const events = vi.hoisted(() => ({ listen: vi.fn() }));
const dialog = vi.hoisted(() => ({ open: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke, Channel: class { onmessage = (_event: unknown) => {}; } }));
vi.mock('@tauri-apps/api/event', () => ({ listen: events.listen }));
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: dialog.open }));

it.each([
  { code: 'download_checkpoint_invalid', message: '本地下载文件或续传断点已缺失或失效，无法继续下载' },
  { code: 'download_engine_unavailable', message: '本地下载引擎不可用，请重启应用后重试' },
  { code: 'download_submission_failed', message: '本地下载引擎未能接受任务，请重试' },
])('preserves local download failure $code instead of replacing it with a network error', async (error) => {
  const api = await import('./cloudApi');
  native.invoke.mockRejectedValueOnce(error);
  await expect(api.downloadCloudFile('pikpak', 'file-id', 'partial.bin')).rejects.toEqual(error);
});

it('shares one authorization listener and releases it after the last subscriber', async () => {
  let receive: ((event: { payload: unknown }) => void) | undefined;
  const stopNative = vi.fn();
  events.listen.mockImplementation(async (_name, callback) => { receive = callback; return stopNative; });
  const api = await import('./cloudApi');
  const first = vi.fn(), second = vi.fn();
  const stopFirst = await api.subscribeCloudAuthorization('quark', first);
  const stopSecond = await api.subscribeCloudAuthorization('uc', second);
  expect(events.listen).toHaveBeenCalledTimes(1);
  const payload = { provider: 'quark', state: 'Ready', account: { provider: 'quark', id: 'q', displayName: 'alice', authMethod: 'web' } };
  receive?.({ payload });
  expect(first).toHaveBeenCalledWith(payload);
  expect(second).not.toHaveBeenCalled();
  stopFirst();
  expect(stopNative).not.toHaveBeenCalled();
  stopSecond(); stopSecond();
  expect(stopNative).toHaveBeenCalledTimes(1);
});

it('queries account-bound cloud capacity and routes capacity invalidations by provider', async () => {
  native.invoke.mockClear();
  events.listen.mockClear();
  const capacity = {
    provider: 'quark',
    accountId: 'account-1',
    capacity: { kind: 'limited', totalBytes: 100, usedBytes: 25 },
    updatedAt: 123
  };
  native.invoke.mockResolvedValueOnce(capacity);
  let receive: ((event: { payload: unknown }) => void) | undefined;
  const stopNative = vi.fn();
  events.listen.mockImplementation(async (_name, callback) => { receive = callback; return stopNative; });
  const api = await import('./cloudApi');

  expect(await api.getCloudAccountCapacity('quark')).toEqual(capacity);
  expect(native.invoke).toHaveBeenCalledWith('cloud_account_capacity', { provider: 'quark' });

  const quark = vi.fn(), baidu = vi.fn();
  const stopQuark = await api.subscribeCloudCapacityInvalidated('quark', quark);
  const stopBaidu = await api.subscribeCloudCapacityInvalidated('baidu', baidu);
  const payload = { provider: 'quark', accountId: 'account-1', reason: 'upload' };
  receive?.({ payload });
  expect(quark).toHaveBeenCalledWith(payload);
  expect(baidu).not.toHaveBeenCalled();
  stopQuark();
  expect(stopNative).not.toHaveBeenCalled();
  stopBaidu();
  expect(stopNative).toHaveBeenCalledTimes(1);
});

it('keeps saved-password metadata separate from explicit and saved-handle login payloads', async () => {
  const api = await import('./cloudApi');
  native.invoke.mockResolvedValueOnce({ username: 'alice', hasSavedPassword: true });
  expect(await api.getSavedCloudCredentials('baidu')).toEqual({ username: 'alice', hasSavedPassword: true });
  await api.loginCloudWithPassword('baidu', { username: 'alice', useSavedPassword: true });
  expect(native.invoke).toHaveBeenLastCalledWith('cloud_login_password', { provider: 'baidu', username: 'alice', useSavedPassword: true });
  await api.loginCloudWithPassword('baidu', { username: 'bob', password: 'new-secret' });
  expect(native.invoke).toHaveBeenLastCalledWith('cloud_login_password', { provider: 'baidu', username: 'bob', password: 'new-secret' });
});

it('signs in to MEGA through its own command, with the second factor only when given', async () => {
  const api = await import('./cloudApi');
  await api.loginMega({ email: 'me@example.com', password: 'secret', secondFactor: '123456' });
  expect(native.invoke).toHaveBeenLastCalledWith('cloud_login_mega', { email: 'me@example.com', password: 'secret', secondFactor: '123456' });
  await api.loginMega({ email: 'me@example.com', password: 'secret' });
  expect(native.invoke).toHaveBeenLastCalledWith('cloud_login_mega', { email: 'me@example.com', password: 'secret' });
});

it('loads the catalog and uses its root when listing without a parent', async () => {
  const api = await import('./cloudApi');
  native.invoke.mockResolvedValueOnce([{ provider: 'baidu', rootDirectoryId: '/dynamic-root' }]);
  native.invoke.mockResolvedValueOnce({ id: '/dynamic-root', items: [] });
  expect(await api.listCloudDirectory('baidu')).toEqual({ id: '/dynamic-root', items: [] });
  expect(native.invoke).toHaveBeenCalledWith('cloud_provider_catalog');
  expect(native.invoke).toHaveBeenCalledWith('cloud_list_directory', { provider: 'baidu', parentId: '/dynamic-root' });
});

it('permanently deletes only the selected cloud recycle-bin files', async () => {
  const api = await import('./cloudApi');

  await api.deleteCloudFiles('pikpak', ['trashed-file-1']);

  expect(native.invoke).toHaveBeenCalledWith('cloud_delete', {
    provider: 'pikpak',
    fileIds: ['trashed-file-1']
  });
});

it('uses the share management command contract for creating, listing, and cancelling links', async () => {
  native.invoke.mockClear();
  native.invoke
    .mockResolvedValueOnce({ id: 'share-1', url: 'https://pan.example/s/share-1', name: '报告.pdf', kind: 'file', sourceFileIds: ['file-1'] })
    .mockResolvedValueOnce([])
    .mockResolvedValueOnce(undefined);
  const api = await import('./cloudApi');
  const options = { name: '报告.pdf', passCode: 'a7b2', expiresInDays: 7 };

  await api.createCloudShare('quark', ['file-1'], options);
  await api.listCloudShares('quark');
  await api.cancelCloudShares('quark', ['share-1']);

  expect(native.invoke).toHaveBeenNthCalledWith(1, 'cloud_create_share', { provider: 'quark', fileIds: ['file-1'], options });
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'cloud_list_shares', { provider: 'quark' });
  expect(native.invoke).toHaveBeenNthCalledWith(3, 'cloud_cancel_shares', { provider: 'quark', shareIds: ['share-1'] });
});

it('opens Baidu operation verification and captures the same account afterward', async () => {
  const api = await import('./cloudApi');
  native.invoke.mockClear();

  await api.openCloudOperationVerification('baidu', 'account-1', 'en-US');
  await api.completeCloudOperationVerification('baidu', 'account-1');

  expect(native.invoke).toHaveBeenNthCalledWith(1, 'cloud_open_operation_verification', {
    provider: 'baidu', expectedAccountId: 'account-1', locale: 'en-US'
  });
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'cloud_complete_operation_verification', {
    provider: 'baidu', expectedAccountId: 'account-1'
  });
});

it('updates the extraction code of an existing share without recreating it', async () => {
  const api = await import('./cloudApi');
  await api.updateCloudSharePassCode('pikpak', 'share-existing', 'a1B2');
  expect(native.invoke).toHaveBeenLastCalledWith('cloud_update_share_pass_code', { provider: 'pikpak', shareId: 'share-existing', passCode: 'a1B2' });
});

it('preserves a duplicate directory outcome with its authoritative group root', async () => {
  native.invoke.mockResolvedValueOnce({
    task: { id: 'directory-1', name: '归档', kind: 'cloud', status: 'completed', progress: 100, duplicate: 'completed_files_present', queueTask: { id: 'group-1', name: '归档', kind: 'folder', status: 'completed', progress: 100, children: [{ id: 'directory-1', name: '归档', kind: 'cloud', status: 'completed', progress: 100 }] } },
    queuedCount: 1,
    failures: [{ relativePath: '归档/私密文件.txt', code: 'network_error', message: '网络或下载引擎错误' }]
  });
  const api = await import('./cloudApi');

  const outcome = await api.downloadCloudDirectory('quark', 'folder-1', '归档');

  expect(native.invoke).toHaveBeenCalledWith('cloud_download_directory', {
    provider: 'quark', directoryId: 'folder-1', directoryName: '归档'
  });
  expect(outcome).toEqual({
    task: { duplicate: { task: { id: 'directory-1', name: '归档', kind: 'cloud', status: 'completed', progress: 100 }, state: 'completed_files_present' }, queueTask: { id: 'group-1', name: '归档', kind: 'folder', status: 'completed', progress: 100, children: [{ id: 'directory-1', name: '归档', kind: 'cloud', status: 'completed', progress: 100 }] } },
    queuedCount: 1,
    failures: [{ relativePath: '归档/私密文件.txt', code: 'network_error', message: '网络或下载引擎错误' }]
  });
});

it('marks cloud file and directory submissions as private only when explicitly requested', async () => {
  native.invoke.mockClear();
  native.invoke.mockResolvedValueOnce({ id: 'private-file' });
  native.invoke.mockResolvedValueOnce({ task: { id: 'private-directory' }, queuedCount: 0, failures: [] });
  const api = await import('./cloudApi');

  await api.downloadCloudFile('quark', 'file-1', 'file.bin', true);
  await api.downloadCloudDirectory('quark', 'folder-1', '归档', true);

  expect(native.invoke).toHaveBeenNthCalledWith(1, 'cloud_download_url', {
    provider: 'quark', fileId: 'file-1', fileName: 'file.bin', privateDownload: true
  });
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'cloud_download_directory', {
    provider: 'quark', directoryId: 'folder-1', directoryName: '归档', privateDownload: true
  });
});

it('sends the selected cloud filename with a file download request', async () => {
  native.invoke.mockClear();
  native.invoke.mockResolvedValueOnce({ id: 'task-1', name: '05_如何制作动感影集.mp4' });
  const api = await import('./cloudApi');

  await api.downloadCloudFile('tianyi', '524751232948846473', '05_如何制作动感影集.mp4');

  expect(native.invoke).toHaveBeenCalledWith('cloud_download_url', {
    provider: 'tianyi',
    fileId: '524751232948846473',
    fileName: '05_如何制作动感影集.mp4'
  });
});

it('invokes file and directory uploads with the structured outcome contract', async () => {
  const outcome = { uploaded: [], failures: [{ itemName: 'bad.txt', code: 'network_error', message: 'offline' }] };
  native.invoke.mockClear();
  native.invoke.mockResolvedValueOnce(outcome).mockResolvedValueOnce(outcome);
  const api = await import('./cloudApi');

  expect(await api.uploadCloudFiles('xunlei', 'root', ['C:\\fixture.txt'])).toEqual(outcome);
  expect(await api.uploadCloudDirectory('xunlei', 'root', 'C:\\fixture')).toEqual(outcome);
  expect(native.invoke).toHaveBeenNthCalledWith(1, 'cloud_upload_files', {
    provider: 'xunlei', parentId: 'root', paths: ['C:\\fixture.txt'], uploadId: expect.any(String), onProgress: expect.any(Object)
  });
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'cloud_upload_directory', {
    provider: 'xunlei', parentId: 'root', path: 'C:\\fixture', uploadId: expect.any(String), onProgress: expect.any(Object)
  });
  expect(native.invoke.mock.calls[0][1].uploadId).not.toBe(native.invoke.mock.calls[1][1].uploadId);
});

it('streams upload progress per invocation before its outcome and ignores late events', async () => {
  const api = await import('./cloudApi');
  let finish!: (value: any) => void;
  native.invoke.mockClear();
  native.invoke.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const receive = vi.fn();
  const pending = api.uploadCloudFiles('quark', '0', ['C:/a.txt'], receive);
  const channel = native.invoke.mock.calls[0][1].onProgress;
  expect(channel).toBeDefined();
  const progress = { uploadId: native.invoke.mock.calls[0][1].uploadId, provider: 'quark', phase: 'uploading', uploadedBytes: 5, totalBytes: 10, speedBytesPerSecond: 5, completedFiles: 0, totalFiles: 1, failedItems: 0, currentFiles: ['a.txt'], progress: 50 };
  channel.onmessage({ ...progress, uploadId: 'different-upload' });
  channel.onmessage({ ...progress, provider: 'pikpak' });
  expect(receive).not.toHaveBeenCalled();
  channel.onmessage(progress);
  expect(receive).toHaveBeenCalledWith(progress);
  const outcome = { uploaded: [], failures: [] };
  finish(outcome);
  expect(await pending).toEqual(outcome);
  channel.onmessage(progress);
  expect(receive).toHaveBeenCalledTimes(1);
});

it('targets upload controls by invocation identity and propagates rejected controls', async () => {
  const api = await import('./cloudApi');
  native.invoke.mockClear();
  native.invoke.mockResolvedValueOnce(undefined).mockResolvedValueOnce(undefined).mockRejectedValueOnce({ code: 'invalid_input', message: '上传已结束' });
  await api.controlCloudUpload('quark', 'upload-1', 'pause');
  await api.controlCloudUpload('quark', 'upload-1', 'resume');
  await expect(api.controlCloudUpload('quark', 'upload-1', 'cancel')).rejects.toEqual({ code: 'invalid_input', message: '上传已结束' });
  expect(native.invoke.mock.calls).toEqual(['pause', 'resume', 'cancel'].map(action => ['cloud_control_upload', { provider: 'quark', uploadId: 'upload-1', action }]));
});

it('selects upload paths before invoking an upload so authorization retries can reuse them', async () => {
  const api = await import('./cloudApi');
  dialog.open
    .mockResolvedValueOnce(['C:\\a.txt', 'C:\\b.txt'])
    .mockResolvedValueOnce('C:\\folder')
    .mockResolvedValueOnce(null);

  expect(await api.pickCloudUploadFiles()).toEqual(['C:\\a.txt', 'C:\\b.txt']);
  expect(await api.pickCloudUploadDirectory()).toBe('C:\\folder');
  expect(await api.pickCloudUploadFiles()).toEqual([]);
  expect(dialog.open).toHaveBeenNthCalledWith(1, { directory: false, multiple: true, title: '选择上传文件' });
  expect(dialog.open).toHaveBeenNthCalledWith(2, { directory: true, multiple: false, title: '选择上传目录' });
});
