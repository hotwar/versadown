import { afterEach, beforeEach, expect, it, vi } from 'vitest';

const native = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke, Channel: class { onmessage = (_message: unknown) => {}; } }));

beforeEach(() => {
  Object.defineProperty(window, '__TAURI_INTERNALS__', {
    configurable: true,
    value: {}
  });
  vi.clearAllMocks();
});

afterEach(() => {
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
});

it('round-trips an optional video cookie path and sends an explicit clear to settings', async () => {
  const { defaultSettings, getSettings, updateSettings } = await import('./api');
  const configured = { ...defaultSettings, videoCookieFile: 'C:\\Cookies\\session.txt' };
  native.invoke.mockResolvedValueOnce(configured);
  await expect(getSettings()).resolves.toEqual(configured);

  const cleared = { ...configured, videoCookieFile: '' };
  native.invoke.mockResolvedValueOnce(cleared);
  await expect(updateSettings(cleared)).resolves.toEqual(cleared);
  expect(native.invoke).toHaveBeenLastCalledWith('update_settings', { settings: cleared });
});

it('localizes cookie validation and access errors from native video commands', async () => {
  const { defaultSettings, updateSettings, parseDownload } = await import('./api');
  native.invoke.mockRejectedValueOnce('video.error.cookieFile');
  await expect(updateSettings({ ...defaultSettings, videoCookieFile: 'missing.txt' })).rejects.toThrow('Cookie 文件无法读取或不是 Netscape cookies.txt 格式');
  native.invoke.mockRejectedValueOnce('video.error.cookies');
  await expect(parseDownload('https://example.test/video', undefined, true, true)).rejects.toThrow('网站需要有效的访问 Cookie');
});

it('classifies ED2K, FTP, HLS and DASH in the browser fallback', async () => {
  const { parseLocally } = await import('./api');

  expect(parseLocally('ed2k://|file|Example%20Archive.zip|42|0123456789ABCDEF0123456789ABCDEF|/')).toMatchObject({
    task: { kind: 'ed2k', name: 'Example Archive.zip' }
  });
  expect(parseLocally('ftp://alice:secret@example.test/pub/archive.zip')).toMatchObject({
    task: { kind: 'ftp', name: 'archive.zip', source: 'ftp://example.test/pub/archive.zip' }
  });
  expect(parseLocally('https://media.test/live/master.m3u8?token=one')).toMatchObject({
    task: { kind: 'hls', name: 'master.m3u8' }
  });
  expect(parseLocally('https://media.test/video/manifest.mpd')).toMatchObject({
    task: { kind: 'dash', name: 'manifest.mpd' }
  });
});

it('maps a share confirmation without inventing an aria2 task', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'share_confirmation',
    plan_source: 'cloud-share://uc/a/b',
    title: '照片',
    resources: [{ id: 'f1', name: '照片/a.jpg', size: '3 B', selected: true }]
  });
  const { parseDownload } = await import('./api');

  await expect(parseDownload('https://drive.uc.cn/s/id')).resolves.toMatchObject({
    confirmation: { taskId: 'cloud-share://uc/a/b', processing: [] }
  });
});

it('maps an access-code requirement without creating a task', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'access_code_required',
    provider: 'baidu',
    message: '请输入提取码后重新解析分享链接'
  });
  const { parseDownload } = await import('./api');

  await expect(parseDownload('https://pan.baidu.com/s/id')).resolves.toEqual({
    shareAccessCodeRequired: {
      provider: 'baidu',
      message: '请输入提取码后重新解析分享链接'
    }
  });
});

it('returns every scanned candidate without creating a download task', async () => {
  native.invoke.mockResolvedValueOnce([
    { input: 'https://example.test/fixture.iso', valid: true, kind: 'direct' },
    { input: 'magnet:?xt=urn:btih:short', valid: false, message: '磁链格式无效' }
  ]);
  const { scanDownloadInputs } = await import('./api');

  await expect(scanDownloadInputs('https://example.test/fixture.iso magnet:?xt=urn:btih:short')).resolves.toEqual([
    { input: 'https://example.test/fixture.iso', valid: true, kind: 'direct' },
    { input: 'magnet:?xt=urn:btih:short', valid: false, message: '磁链格式无效' }
  ]);
  expect(native.invoke).toHaveBeenCalledWith('scan_download_inputs', {
    input: 'https://example.test/fixture.iso magnet:?xt=urn:btih:short'
  });
});

it('keeps an ordinary direct link valid in the browser scan fallback', async () => {
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
  const { scanDownloadInputs } = await import('./api');

  await expect(scanDownloadInputs('https://example.test/archive.zip')).resolves.toEqual([
    { input: 'https://example.test/archive.zip', valid: true, kind: 'direct' }
  ]);
});

it('passes a transient access code only to the parse command', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'access_code_required',
    provider: 'baidu',
    message: '请输入提取码后重新解析分享链接'
  });
  const { parseDownload } = await import('./api');

  await parseDownload('https://pan.baidu.com/s/id', '1234');

  expect(native.invoke).toHaveBeenCalledWith('parse_download', { input: 'https://pan.baidu.com/s/id', accessCode: '1234' });
});

it('prepares every candidate without starting it and discards an abandoned native draft', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'task',
    task: { id: 'task-1', name: 'archive.zip', kind: 'file', status: 'awaiting_user', progress: 0 },
    requires_confirmation: true,
    resources: []
  }).mockResolvedValueOnce(undefined);
  const { discardPreparedDownload, parseDownload } = await import('./api');

  await parseDownload('https://example.test/archive.zip', undefined, true);
  await discardPreparedDownload('task-1');

  expect(native.invoke).toHaveBeenNthCalledWith(1, 'parse_download', {
    input: 'https://example.test/archive.zip',
    requireConfirmation: true
  });
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'discard_prepared_download', { taskId: 'task-1' });
});

it('treats an indexed magnet file list as ready even while aria2 metadata is unresolved', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'task',
    task: {
      id: 'magnet-preview', name: 'large torrent', kind: 'magnet', status: 'awaiting_user', progress: 0,
      aria_gid: 'metadata-gid', metadata_gid: 'metadata-gid',
    },
    requires_confirmation: true,
    resources: [{ id: '1', name: 'large torrent/preview.bin', size: '1 B', selected: true }],
  });
  const { parseDownload } = await import('./api');

  await expect(parseDownload('magnet:?xt=urn:btih:large', undefined, true)).resolves.toMatchObject({
    confirmation: {
      taskId: 'magnet-preview',
      isBitTorrent: true,
      fileListPending: false,
      resources: [{ id: '1', name: 'large torrent/preview.bin' }],
    },
  });
});

it('stages dropped torrent bytes for the normal confirmation flow', async () => {
  native.invoke.mockResolvedValueOnce('C:\\AppData\\dropped-torrents\\fixture.torrent');
  const { stageDroppedTorrent } = await import('./api');

  await expect(stageDroppedTorrent('fixture.torrent', new Uint8Array([1, 2, 3]))).resolves.toBe('C:\\AppData\\dropped-torrents\\fixture.torrent');
  expect(native.invoke).toHaveBeenCalledWith('stage_dropped_torrent', {
    fileName: 'fixture.torrent',
    contentsBase64: 'AQID',
  });
});

it('marks an empty magnet file list as pending until a complete list is available', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'task',
    task: {
      id: 'magnet-waiting', name: 'large torrent', kind: 'magnet', status: 'awaiting_user', progress: 0,
      aria_gid: 'metadata-gid', metadata_gid: 'metadata-gid',
    },
    requires_confirmation: true,
    resources: [],
  });
  const { parseDownload } = await import('./api');

  await expect(parseDownload('magnet:?xt=urn:btih:waiting', undefined, true)).resolves.toMatchObject({
    confirmation: { fileListPending: true },
  });
});

it('keeps a followed magnet file list ready after aria2 creates its child', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'task',
    task: {
      id: 'magnet-complete', name: 'large torrent', kind: 'magnet', status: 'awaiting_user', progress: 0,
      aria_gid: 'download-gid', followed_by_gid: 'download-gid', metadata_gid: 'metadata-gid',
    },
    requires_confirmation: true,
    resources: [{ id: '137', name: 'large torrent/complete.bin', size: '1 B', selected: true }],
  });
  const { parseDownload } = await import('./api');

  await expect(parseDownload('magnet:?xt=urn:btih:complete', undefined, true)).resolves.toMatchObject({
    confirmation: { fileListPending: false },
  });
});

it('sends opaque share plans to the cloud-share confirmation command', async () => {
  native.invoke.mockResolvedValueOnce({ outcomes: [], failures: [] });
  const { confirmCloudShareDownload } = await import('./api');

  await confirmCloudShareDownload(`cloud-share://baidu/${'a'.repeat(64)}/${'b'.repeat(64)}`, ['f1']);

  expect(native.invoke).toHaveBeenCalledWith('confirm_cloud_share_download', {
    planSource: `cloud-share://baidu/${'a'.repeat(64)}/${'b'.repeat(64)}`,
    selectedIds: ['f1'], onCreated: expect.any(Object)
  });
});

it('delivers the durable named task before the batch command resolves', async () => {
  let finish!: (value: unknown) => void;
  const created: unknown[] = [];
  native.invoke.mockImplementationOnce((_command, args) => {
    args.onCreated.onmessage({ id: 'root', name: '原始目录', kind: 'folder', status: 'queued', progress: 0 });
    return new Promise(resolve => { finish = resolve; });
  });
  const { confirmCloudShareDownload } = await import('./api');
  const pending = confirmCloudShareDownload('cloud-share://pikpak/plan', ['file'], undefined, false, task => created.push(task));
  expect(created).toEqual([{ id: 'root', name: '原始目录', kind: 'folder', status: 'queued', progress: 0 }]);
  finish({ outcomes: [], failures: [] });
  await pending;
});

it('preserves every native share task outcome and maps duplicate metadata in order', async () => {
  native.invoke.mockResolvedValueOnce({
    outcomes: [
      { id: 'queued-1', name: 'first.bin', kind: 'cloud', status: 'queued', progress: 0 },
      { id: 'duplicate-1', name: 'existing.bin', kind: 'cloud', status: 'completed', progress: 100, duplicate: 'completed_files_present' },
      { id: 'queued-2', name: 'second.bin', kind: 'cloud', status: 'queued', progress: 0 }
    ],
    failures: [{ selectedId: 'f4', code: 'web_auth_required', message: '网盘需要网页验证' }]
  });
  const { confirmCloudShareDownload } = await import('./api');

  await expect(confirmCloudShareDownload('cloud-share://baidu/plan', ['f1', 'f2', 'f3'])).resolves.toEqual({
    outcomes: [
      { task: { id: 'queued-1', name: 'first.bin', kind: 'cloud', status: 'queued', progress: 0 } },
      {
        duplicate: {
          task: { id: 'duplicate-1', name: 'existing.bin', kind: 'cloud', status: 'completed', progress: 100 },
          state: 'completed_files_present'
        }
      },
      { task: { id: 'queued-2', name: 'second.bin', kind: 'cloud', status: 'queued', progress: 0 } }
    ],
    failures: [{ selectedId: 'f4', code: 'web_auth_required', message: '网盘需要网页验证' }]
  });
});

it('maps the tagged native task outcome without changing direct downloads', async () => {
  native.invoke.mockResolvedValueOnce({
    kind: 'task',
    task: { id: 'task-1', name: 'fixture.bin', kind: 'file', status: 'downloading', progress: 0 },
    requires_confirmation: false,
    resources: []
  });
  const { parseDownload } = await import('./api');

  await expect(parseDownload('https://example.test/fixture.bin')).resolves.toEqual({
    task: { id: 'task-1', name: 'fixture.bin', kind: 'file', status: 'downloading', progress: 0 }
  });
});

it('sends the privacy confirmation and private selection flags only when requested', async () => {
  native.invoke.mockResolvedValueOnce({ kind: 'task', task: { id: 'awaiting', name: 'fixture.bin', kind: 'file', status: 'awaiting_user', progress: 0 }, requires_confirmation: true, resources: [] });
  native.invoke.mockResolvedValueOnce({ id: 'private-direct' });
  native.invoke.mockResolvedValueOnce({ outcomes: [], failures: [] });
  const { confirmCloudShareDownload, confirmDownload, parseDownload } = await import('./api');

  await parseDownload('https://example.test/fixture.bin', undefined, true);
  await confirmDownload('awaiting', [], undefined, true);
  await confirmCloudShareDownload('cloud-share://quark/plan', ['f1'], undefined, true);

  expect(native.invoke).toHaveBeenNthCalledWith(1, 'parse_download', {
    input: 'https://example.test/fixture.bin', accessCode: undefined, requireConfirmation: true
  });
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'confirm_download_command', {
    taskId: 'awaiting', selectedIds: [], privateDownload: true
  });
  expect(native.invoke).toHaveBeenNthCalledWith(3, 'confirm_cloud_share_download', {
    planSource: 'cloud-share://quark/plan', selectedIds: ['f1'], privateDownload: true, onCreated: expect.any(Object)
  });
});

it('sends a dedicated privacy-move request with the exact task payload', async () => {
  native.invoke.mockResolvedValueOnce({ id: 'task-1', is_private: true });
  const { setTaskPrivacy } = await import('./api');

  await expect(setTaskPrivacy('task-1', true)).resolves.toEqual({ id: 'task-1', is_private: true });
  expect(native.invoke).toHaveBeenCalledWith('set_task_private', { taskId: 'task-1', isPrivate: true });
});

it('passes one shared post-processing note to native and cloud confirmations', async () => {
  native.invoke.mockResolvedValueOnce({ id: 'native' });
  native.invoke.mockResolvedValueOnce({ outcomes: [], failures: [] });
  const { confirmCloudShareDownload, confirmDownload } = await import('./api');

  await confirmDownload('native', ['1'], 'D:\\Downloads', false, '密码 88in\n完成后拼接');
  await confirmCloudShareDownload('cloud-share://baidu/plan', ['f1'], undefined, false, undefined, '密码 88in\n完成后拼接');

  expect(native.invoke).toHaveBeenNthCalledWith(1, 'confirm_download_command', {
    taskId: 'native', selectedIds: ['1'], destinationDirectory: 'D:\\Downloads',
    postProcessingNote: '密码 88in\n完成后拼接'
  });
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'confirm_cloud_share_download', {
    planSource: 'cloud-share://baidu/plan', selectedIds: ['f1'],
    postProcessingNote: '密码 88in\n完成后拼接', onCreated: expect.any(Object)
  });
});

it.each([
  ['metadata_expired', /XL-LOCAL-EXPIRED/],
  ['metadata_missing', /XL-LOCAL-MISSING/],
  ['timestamp_invalid', /XL-LOCAL-TIME/],
  ['xunlei_share_profile_unsupported', /XL-SHARE-PROFILE/],
  ['brand_new_reason', /XL-LOCAL\]/]
])('localizes the Xunlei verification code %s instead of showing it', async (code, expected) => {
  native.invoke.mockRejectedValueOnce(code === 'brand_new_reason' ? { code: 'xunlei_verification_unavailable', message: code } : code);
  const { parseDownload } = await import('./api');

  const failure = await parseDownload('https://pan.xunlei.com/s/abc').catch((error: Error) => error);

  expect(failure).toBeInstanceOf(Error);
  expect((failure as Error).message).not.toContain(code);
  expect((failure as Error).message).toMatch(expected);
});

it('submits only native preview and format IDs and releases a fully submitted preview', async () => {
  const { parseDownload, submitVideoFormats } = await import('./api');
  const format = (id: string, width: number, height: number) => ({ id, width, height, ext: 'mp4', bytes: null, estimated: true });
  const vid = { kind: 'web_video', previewId: 'native-preview', pageUrl: 'https://vimeo.com/12345', site: 'Vimeo', title: 'At the zoo',
    duration: 19, thumbnail: null, formats: [format('18', 320, 240),format('160+140', 192, 144)] };
  native.invoke.mockResolvedValueOnce(vid);
  const {confirmation} = await parseDownload(vid.pageUrl, undefined, true, true);
  expect(native.invoke).toHaveBeenCalledWith('parse_download', {input: vid.pageUrl, accessCode: undefined, requireConfirmation:true, probeVideo:true});
  expect(confirmation?.resources.map(r => r.selected)).toEqual([true,false]);
  expect(confirmation?.taskId).toBe('video-page:native-preview');
  const created = vi.fn();
  native.invoke.mockResolvedValueOnce({ id: 'task-video', kind:'web_video', status:'queued' });
  expect(await submitVideoFormats(confirmation!.video!, ['160+140'], '/movies', false, created)).toEqual([]);
  expect(native.invoke).toHaveBeenNthCalledWith(2, 'submit_video_download', {
    previewId:'native-preview', formatId:'160+140', destinationDirectory:'/movies', privateDownload:false, postProcessingNote:undefined,
  });
  expect(native.invoke).toHaveBeenNthCalledWith(3, 'discard_video_preview', { previewId:'native-preview' });
  expect(created).toHaveBeenCalledWith(expect.objectContaining({id:'task-video'}));
});
