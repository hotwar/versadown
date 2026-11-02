import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { readFileSync } from 'node:fs';
import { vi } from 'vitest';
import { CloudManager as NativeCloudManager, type CloudManagerProps } from './CloudManager';
import type { CloudProvider } from '../lib/cloudTypes';
import { cloudCatalog, cloudRecycleBinDirectoryId, cloudRootDirectoryId } from '../test/cloudCatalog';
import { setLocalePreference, t } from '../i18n';

function CloudManager(props: Omit<CloudManagerProps, 'descriptor'>) {
  return <NativeCloudManager descriptor={cloudCatalog.find(d => d.provider === props.provider)!} {...props} />;
}

async function confirmAllLocalUploads(user: ReturnType<typeof userEvent.setup>) {
  const picker = await screen.findByRole('dialog', { name: '选择上传内容' });
  await user.click(await within(picker).findByRole('checkbox', { name: '全选当前目录' }));
  await user.click(within(picker).getByRole('button', { name: '确认上传' }));
}

it('refreshes a cached directory when its account is renewed externally', async () => {
  const client = createCloudFake();
  client.listCloudDirectory.mockRejectedValueOnce({ code: 'network_error', message: '会话连接已断开' });
  const account = { id: 'q', provider: 'quark' as const, displayName: 'alice', authMethod: 'web' as const };
  const view = render(<CloudManager provider="quark" client={client} initialAccount={account} restoreOnMount={false} showSessionActions={false} />);
  await screen.findByText('会话连接已断开');
  view.rerender(<CloudManager provider="quark" client={client} initialAccount={{ ...account }} restoreOnMount={false} showSessionActions={false} />);
  expect(await screen.findByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
});

it('shows the preparing notice before the first upload progress in a non-Chinese locale', async () => {
  await setLocalePreference('en');
  try {
    const user = userEvent.setup();
    const client = createCloudFake({ uploadCloudFiles: vi.fn(() => new Promise(() => undefined)) });
    render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
    await screen.findByRole('tree');
    await user.click(screen.getByRole('button', { name: t('cloud.action.upload'), exact: true }));
    const picker = await screen.findByRole('dialog', { name: t('upload.selectContent') });
    await user.click(await within(picker).findByRole('checkbox', { name: t('upload.selectCurrentFolder') }));
    await user.click(within(picker).getByRole('button', { name: t('upload.confirm') }));
    expect(await screen.findByText(t('cloud.upload.preparing'))).toBeVisible();
  } finally {
    await setLocalePreference('auto');
  }
});

it('shows live upload bytes, speed and confirmation phase in the existing operation notice', async () => {
  const user = userEvent.setup();
  let receive: ((value: any) => void) | undefined;
  let finish!: (value: any) => void;
  const client = createCloudFake({ uploadCloudFiles: vi.fn((_p, _d, _paths, progress) => {
    receive = progress;
    return new Promise(resolve => { finish = resolve; });
  }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await confirmAllLocalUploads(user);
  const progress = { provider: 'quark', phase: 'uploading', totalBytes: 1048576, uploadedBytes: 524288, speedBytesPerSecond: 262144, totalFiles: 2, completedFiles: 1, failedItems: 0, currentFiles: ['b.txt'], progress: 50 };
  await act(async () => { receive?.(progress); });
  const bar = screen.getByRole('progressbar', { name: '上传总进度' });
  expect(bar).toHaveAttribute('aria-valuenow', '50');
  const notice = bar.closest('.cloud-operation-status')!;
  expect(notice).toHaveTextContent('512.0 KB / 1.0 MB');
  expect(notice).toHaveTextContent('256.0 KB/s');
  expect(notice).toHaveTextContent('b.txt');
  expect(within(notice).queryByText('上传进行中…')).not.toBeInTheDocument();
  const stats = within(notice).getByText(/50% ·/).parentElement!;
  expect(within(stats).getByText('已完成 1 / 2 个文件，失败 0 项')).toBeVisible();
  await act(async () => { receive?.({ ...progress, phase: 'finalizing', uploadedBytes: 1048576, speedBytesPerSecond: 0, progress: 99 }); });
  expect(notice).toHaveTextContent('等待网盘确认');
  expect(bar).toHaveAttribute('aria-valuenow', '99');
  await act(async () => { finish({ uploaded: [{ id: 'new', name: 'a.txt', parentId: '0', kind: 'file' }], failures: [] }); });
  expect(await screen.findByText('已上传 1 项')).toBeVisible();
  await act(async () => { receive?.(progress); });
  expect(screen.getByText('已上传 1 项')).toBeVisible();
  expect(screen.queryByRole('progressbar')).not.toBeInTheDocument();
});

it('controls the current upload with pause resume and cancel and preserves completed items', async () => {
  const user = userEvent.setup();
  let receive!: (value: any) => void;
  let finish!: (value: any) => void;
  let acknowledge!: () => void;
  const controlCloudUpload = vi.fn().mockImplementationOnce(() => new Promise<void>(resolve => { acknowledge = resolve; })).mockResolvedValue(undefined);
  const client = createCloudFake({ controlCloudUpload, uploadCloudFiles: vi.fn((_p, _d, _paths, callback) => { receive = callback; return new Promise(resolve => { finish = resolve; }); }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await confirmAllLocalUploads(user);
  const progress = { uploadId: 'upload-1', provider: 'quark', phase: 'uploading', totalBytes: 100, uploadedBytes: 40, speedBytesPerSecond: 10, totalFiles: 2, completedFiles: 1, failedItems: 0, currentFiles: ['b.txt'], progress: 40 };
  await act(async () => { receive(progress); });
  const trackRow = screen.getByRole('progressbar').parentElement!;
  const pauseButton = within(trackRow).getByRole('button', { name: '暂停上传' });
  const cancelButton = within(trackRow).getByRole('button', { name: '取消上传' });
  expect(pauseButton).toHaveClass('cloud-upload-control-button');
  expect(pauseButton).toHaveTextContent('');
  expect(pauseButton.querySelector('svg')).toBeVisible();
  expect(cancelButton).toHaveClass('cloud-upload-control-button');
  expect(cancelButton).toHaveTextContent('');
  expect(cancelButton.querySelector('svg')).toBeVisible();
  await user.click(pauseButton);
  expect(screen.getByRole('button', { name: '暂停上传' })).toBeDisabled();
  expect(screen.queryByRole('button', { name: '继续上传' })).not.toBeInTheDocument();
  await act(async () => { acknowledge(); });
  const resumeButton = await screen.findByRole('button', { name: '继续上传' });
  expect(screen.getByText(/10 B\/s/)).toBeVisible();
  expect(resumeButton).toHaveTextContent('');
  expect(resumeButton.querySelector('svg')).toBeVisible();
  await user.click(resumeButton);
  expect(screen.getByText(/10 B\/s/)).toBeVisible();
  await act(async () => { receive({ ...progress, speedBytesPerSecond: 25 }); });
  expect(screen.getByText(/25 B\/s/)).toBeVisible();
  await user.click(await screen.findByRole('button', { name: '取消上传' }));
  expect(controlCloudUpload.mock.calls).toEqual(['pause', 'resume', 'cancel'].map(action => ['quark', 'upload-1', action]));
  expect(await screen.findByRole('button', { name: '正在取消上传…' })).toBeDisabled();
  await act(async () => { receive({ ...progress, phase: 'cancelled', speedBytesPerSecond: 0 }); finish({ uploaded: [{ id: 'done', name: 'a.txt', kind: 'file', parentId: '0' }], failures: [{ inputIndex: 1, itemName: 'b.txt', code: 'upload_cancelled', message: '上传已取消' }], cancelled: true }); });
  expect(await screen.findByText('上传已取消，已保留 1 项上传结果。')).toBeVisible();
  expect(screen.queryByRole('button', { name: '暂停上传' })).not.toBeInTheDocument();
  expect(screen.queryByText(/失败 1 项/)).not.toBeInTheDocument();
});

it('keeps real upload progress and offers control retry when pause fails', async () => {
  const user = userEvent.setup();
  let receive!: (value: any) => void;
  const client = createCloudFake({ controlCloudUpload: vi.fn().mockRejectedValue(new Error('控制请求失败')), uploadCloudFiles: vi.fn((_p, _d, _paths, callback) => { receive = callback; return new Promise(() => {}); }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await confirmAllLocalUploads(user);
  await act(async () => { receive({ uploadId: 'upload-1', provider: 'quark', phase: 'uploading', totalBytes: 100, uploadedBytes: 40, speedBytesPerSecond: 10, totalFiles: 2, completedFiles: 1, failedItems: 0, currentFiles: ['b.txt'], progress: 40 }); });
  await user.click(screen.getByRole('button', { name: '暂停上传' }));
  expect(await screen.findByText('控制请求失败')).toBeVisible();
  expect(screen.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '40');
  expect(screen.getByRole('button', { name: '暂停上传' })).toBeEnabled();
  expect(screen.queryByRole('button', { name: '继续上传' })).not.toBeInTheDocument();
});

it('ignores a late control failure after the upload terminal event arrives', async () => {
  const user = userEvent.setup();
  let receive!: (value: any) => void;
  let rejectControl!: (error: Error) => void;
  const client = createCloudFake({ controlCloudUpload: vi.fn(() => new Promise((_resolve, reject) => { rejectControl = reject; })), uploadCloudFiles: vi.fn((_p, _d, _paths, callback) => { receive = callback; return new Promise(() => {}); }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await confirmAllLocalUploads(user);
  const progress = { uploadId: 'upload-1', provider: 'quark', phase: 'uploading', totalBytes: 100, uploadedBytes: 40, speedBytesPerSecond: 10, totalFiles: 1, completedFiles: 0, failedItems: 0, currentFiles: ['a.txt'], progress: 40 };
  await act(async () => { receive(progress); });
  await user.click(screen.getByRole('button', { name: '暂停上传' }));
  await act(async () => { receive({ ...progress, phase: 'completed', uploadedBytes: 100, completedFiles: 1, progress: 100, currentFiles: [], speedBytesPerSecond: 0 }); });
  await act(async () => { rejectControl(new Error('上传任务已结束或不存在')); });
  expect(screen.getByText('上传已完成')).toBeVisible();
  expect(screen.queryByText('上传任务已结束或不存在')).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '暂停上传' })).not.toBeInTheDocument();
});

it('resumes only failed upload inputs after the cached account is renewed externally', async () => {
  const user = userEvent.setup();
  const uploadCloudFiles = vi.fn().mockResolvedValueOnce({ uploaded: [], failures: [{ inputIndex: 0, itemName: 'a.txt', code: 'web_auth_required', message: '需要验证' }] })
    .mockResolvedValue({ uploaded: [{ id: 'new', name: 'a.txt', kind: 'file' }], failures: [] });
  const client = createCloudFake({ uploadCloudFiles });
  const account = { id: 'q', provider: 'quark' as const, displayName: 'alice', authMethod: 'web' as const };
  const view = render(<CloudManager provider="quark" client={client} initialAccount={account} restoreOnMount={false} showSessionActions={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await confirmAllLocalUploads(user);
  await screen.findByRole('button', { name: /授权并继续上传/ });
  view.rerender(<CloudManager provider="quark" client={client} initialAccount={{ ...account }} restoreOnMount={false} showSessionActions={false} />);
  await waitFor(() => expect(uploadCloudFiles).toHaveBeenCalledTimes(2));
  expect(uploadCloudFiles).toHaveBeenLastCalledWith('quark', '0', ['C:/a.txt'], expect.any(Function));
  expect(await screen.findByText('已上传 1 项')).toBeVisible();
  expect(screen.queryByRole('button', { name: /授权并继续上传|重新网页授权/ })).not.toBeInTheDocument();
});

it('retains the upload outcome even if refreshing the remote directory fails', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ uploadCloudFiles: vi.fn().mockResolvedValue({ uploaded: [{ id: 'new', name: 'a.txt', kind: 'file', parentId: '0' }], failures: [] }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree');
  client.listCloudDirectory.mockRejectedValue(new Error('列表请求超时'));
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await confirmAllLocalUploads(user);
  expect(await screen.findByText('已上传 1 项')).toBeVisible();
  expect(screen.getByText('列表请求超时')).toBeVisible();
});

it('consumes external authorization received during upload after a delayed directory refresh settles', async () => {
  const user = userEvent.setup();
  let finishUpload!: (value: any) => void;
  let finishRefresh!: (value: any) => void;
  const uploadCloudFiles = vi.fn().mockImplementationOnce(() => new Promise(resolve => { finishUpload = resolve; }))
    .mockResolvedValue({ uploaded: [{ id: 'new', name: 'a.txt', kind: 'file' }], failures: [] });
  const client = createCloudFake({ uploadCloudFiles });
  const account = { id: 'q', provider: 'quark' as const, displayName: 'alice', authMethod: 'web' as const };
  const view = render(<CloudManager provider="quark" client={client} initialAccount={account} restoreOnMount={false} showSessionActions={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await confirmAllLocalUploads(user);
  view.rerender(<CloudManager provider="quark" client={client} initialAccount={{ ...account }} restoreOnMount={false} showSessionActions={false} />);
  client.listCloudDirectory.mockImplementationOnce(() => new Promise(resolve => { finishRefresh = resolve; }));
  await act(async () => { finishUpload({ uploaded: [], failures: [{ inputIndex: 0, itemName: 'a.txt', code: 'web_auth_required', message: '需要验证' }] }); });
  await act(async () => { finishRefresh({ id: '0', items: [] }); });
  await waitFor(() => expect(uploadCloudFiles).toHaveBeenCalledTimes(2));
  expect(await screen.findByText('已上传 1 项')).toBeVisible();
});

it('opens one neutral upload action and submits mixed local paths to the current directory', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({
    listLocalUploadEntries: vi.fn().mockResolvedValue({ path: 'C:/Uploads', parentPath: 'C:/', roots: [], entries: [
      { path: 'C:/Uploads/a.txt', name: 'a.txt', kind: 'file' },
      { path: 'C:/Uploads/Photos', name: 'Photos', kind: 'directory' }
    ] }),
    uploadCloudFiles: vi.fn().mockResolvedValue({ uploaded: [], failures: [] })
  });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.dblClick(screen.getByText('归档'));
  await waitFor(() => expect(client.listCloudDirectory).toHaveBeenLastCalledWith('quark', 'folder-1'));
  const upload = screen.getByRole('button', { name: '上传', exact: true });
  expect(upload).toHaveTextContent('上传');
  expect(upload).not.toHaveClass('cloud-toolbar-primary');
  expect(screen.queryByRole('button', { name: '上传目录', exact: true })).not.toBeInTheDocument();
  await user.click(upload);
  expect(await screen.findByRole('dialog', { name: '选择上传内容' })).toBeVisible();
  await user.click(await screen.findByRole('checkbox', { name: '选择 a.txt' }));
  await user.click(screen.getByRole('checkbox', { name: '选择 Photos' }));
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  await waitFor(() => expect(client.uploadCloudFiles).toHaveBeenCalledWith('quark', 'folder-1', ['C:/Uploads/a.txt', 'C:/Uploads/Photos'], expect.any(Function)));
  expect(screen.queryByRole('dialog', { name: '选择上传内容' })).not.toBeInTheDocument();
  expect(client.pickAndUploadFiles).not.toHaveBeenCalled();
  expect(client.pickAndUploadDirectory).not.toHaveBeenCalled();
});

it('review shares an overlapping begin and does not accept an event before begin registers', async () => {
  let finishBegin!: (value: any) => void;
  let receive: ((event: any) => void) | undefined;
  const onAccountAuthorized = vi.fn();
  const account = { provider: 'quark', id: 'current', displayName: 'Current', authMethod: 'web' };
  const client = createCloudFake({
    beginCloudAuthorization: vi.fn(() => new Promise(resolve => { finishBegin = resolve; })),
    subscribeCloudAuthorization: vi.fn(async (_provider, callback) => { receive = callback; return vi.fn(); }),
    getCompletedCloudAuthorization: vi.fn().mockResolvedValue(account),
  });
  render(<CloudManager provider="quark" client={client} authorizationOnly onAccountAuthorized={onAccountAuthorized} />);
  await act(async () => {});
  fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
  fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
  await act(async () => {});
  expect(client.beginCloudAuthorization).toHaveBeenCalledTimes(1);
  await act(async () => { receive?.({ provider: 'quark', state: 'Ready', account: { ...account, id: 'old' } }); });
  expect(onAccountAuthorized).not.toHaveBeenCalled();
  await act(async () => { finishBegin({ provider: 'quark', windowLabel: 'auth', instructions: '等待' }); });
  await act(async () => { receive?.({ provider: 'quark', state: 'Ready', account }); });
  expect(onAccountAuthorized).toHaveBeenCalledWith(account);
  expect(screen.queryByRole('button', { name: '我已完成授权' })).not.toBeInTheDocument();
});

it('review ignores an old completion query after a newer authorization attempt starts', async () => {
  let finishOld!: (account: any) => void;
  const oldQuery = new Promise(resolve => { finishOld = resolve; });
  const account = { provider: 'xunlei', id: 'new', displayName: 'New', authMethod: 'web' };
  const client = createCloudFake({ getCompletedCloudAuthorization: vi.fn().mockReturnValueOnce(oldQuery).mockResolvedValue(account), completeCloudAuthorization: vi.fn().mockRejectedValue({ code: 'web_auth_required', message: '需要重新授权' }) });
  const accepted = vi.fn();
  render(<CloudManager provider="xunlei" client={client} authorizationOnly onAccountAuthorized={accepted} />);
  await act(async () => {});
  fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
  await act(async () => {});
  await act(async () => { client.emitAuthorization({ ...account, id: 'old' }); });
  fireEvent.click(screen.getByRole('button', { name: '我已完成授权' }));
  await act(async () => {});
  fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
  await act(async () => {});
  await act(async () => { finishOld({ ...account, id: 'old' }); });
  expect(accepted).not.toHaveBeenCalled();
  expect(screen.getByRole('button', { name: '我已完成授权' })).toBeVisible();
  await act(async () => { client.emitAuthorization(account); });
  expect(accepted).toHaveBeenCalledTimes(1);
  expect(accepted).toHaveBeenCalledWith(account);
});

it('accepts authorization events immediately and ignores duplicate or unrelated completion', async () => {
  vi.useFakeTimers();
  try {
    let receive: ((event: any) => void) | undefined;
    const unlisten = vi.fn();
    const onAccountAuthorized = vi.fn();
    const client = createCloudFake({ getCompletedCloudAuthorization: vi.fn().mockResolvedValue({ id: 'q', provider: 'quark', displayName: 'alice', authMethod: 'web' }), subscribeCloudAuthorization: vi.fn(async (_provider, callback) => { receive = callback; return unlisten; }) });
    const view = render(<CloudManager provider="quark" client={client} onAccountAuthorized={onAccountAuthorized} />);
    await act(async () => {});
    fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
    await act(async () => {});
    await act(async () => { receive?.({ provider: 'uc', state: 'Ready', account: { id: 'other', provider: 'uc', displayName: 'other', authMethod: 'web' } }); });
    expect(screen.queryByRole('tree')).not.toBeInTheDocument();
    const event = { provider: 'quark', state: 'Ready', account: { id: 'q', provider: 'quark', displayName: 'alice', authMethod: 'web' } };
    await act(async () => { receive?.(event); receive?.(event); });
    expect(screen.getByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
    expect(screen.queryByRole('button', { name: '我已完成授权' })).not.toBeInTheDocument();
    expect(onAccountAuthorized).toHaveBeenCalledTimes(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
    expect(unlisten).toHaveBeenCalledTimes(1);
    view.unmount();
  } finally { vi.useRealTimers(); }
});

it('uses a ten second serial watchdog and stops it on unmount when events are lost', async () => {
  vi.useFakeTimers();
  try {
    const client = createCloudFake({ completeCloudAuthorization: vi.fn().mockRejectedValue({ code: 'authorization_pending' }) });
    const view = render(<CloudManager provider="quark" client={client} />);
    await act(async () => {});
    fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
    await act(async () => {});
    await act(async () => { await vi.advanceTimersByTimeAsync(9_999); });
    expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
    await act(async () => { await vi.advanceTimersByTimeAsync(1); });
    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(1);
    view.unmount();
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(1);
  } finally { vi.useRealTimers(); }
});

it('uses catalog authorization and directory identifiers instead of provider defaults', async () => {
  const client = createCloudFake();
  const descriptor = { ...cloudCatalog[0], displayName: '目录名称', rootDirectoryId: 'custom-root', recycleBinDirectoryId: 'custom-trash', authorization: { provider: 'quark' as const, web: false, password: true, passwordLoginMode: 'rustDirect' as const } };
  const { rerender } = render(<NativeCloudManager descriptor={descriptor} provider="quark" client={client} authorizationOnly />);
  expect(screen.getByRole('region', { name: '夸克网盘账号授权' })).toBeVisible();
  expect(screen.getByLabelText('账号')).toBeVisible();
  expect(screen.queryByRole('button', { name: '使用网页授权' })).not.toBeInTheDocument();
  rerender(<NativeCloudManager key="files" descriptor={descriptor} provider="quark" client={client} initialAccount={{ id: 'q1', provider: 'quark', displayName: 'q', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree', { name: '夸克网盘文件' });
  expect(client.listCloudDirectory).toHaveBeenCalledWith('quark', 'custom-root');
  expect(screen.getByRole('treeitem', { name: /回收站/ })).toBeVisible();
});

function createCloudFake(overrides: Record<string, unknown> = {}) {
  const listeners = new Set<(event: any) => void>();
  const capacityListeners = new Set<(event: any) => void>();
  let completedAccount: any = null;
  return {
    getCompletedCloudAuthorization: vi.fn(async () => completedAccount),
    subscribeCloudAuthorization: vi.fn(async (_provider: CloudProvider, receive: (event: any) => void) => { listeners.add(receive); return () => { listeners.delete(receive); }; }),
    emitAuthorization: (account: any) => { completedAccount = account; listeners.forEach(receive => receive({ provider: account.provider, state: 'Ready', account })); },
    emitCapacityInvalidated: (event: any) => { capacityListeners.forEach(receive => receive(event)); },
    getCloudProviderCatalog: vi.fn().mockResolvedValue(cloudCatalog),
    beginCloudAuthorization: vi.fn().mockImplementation((provider: CloudProvider) => Promise.resolve({ provider, windowLabel: '网盘登录', instructions: '请在安全窗口中完成登录。' })),
    completeCloudAuthorization: vi.fn().mockImplementation((provider: CloudProvider) => Promise.resolve({ id: 'account-1', displayName: 'alice', provider, authMethod: 'web' as const })),
    loginCloudWithPassword: vi.fn().mockImplementation((provider: CloudProvider) => Promise.resolve({ id: 'account-1', displayName: 'alice', provider, authMethod: 'password' as const })),
    getSavedCloudCredentials: vi.fn().mockResolvedValue(null),
    getCloudAccount: vi.fn().mockRejectedValue(Object.assign(new Error('未授权'), { code: 'unauthorized' })),
    getCloudAccountCapacity: vi.fn().mockImplementation((provider: CloudProvider) => Promise.resolve({
      provider, accountId: `${provider}-account`, capacity: { kind: 'limited', totalBytes: 100 * 1024 ** 3, usedBytes: 12 * 1024 ** 3 }, updatedAt: 1
    })),
    subscribeCloudCapacityInvalidated: vi.fn(async (_provider: CloudProvider, receive: (event: any) => void) => { capacityListeners.add(receive); return () => { capacityListeners.delete(receive); }; }),
    listCloudDirectory: vi.fn().mockImplementation((_provider: string, parentId = '0') => Promise.resolve(parentId === 'folder-1'
      ? { id: 'folder-1', items: [{ id: 'child-1', parentId: 'folder-1', name: '说明.txt', kind: 'file', size: 1024, modifiedAt: '2025-06-07 08:09:10' }] }
      : parentId === 'folder-2'
        ? { id: 'folder-2', items: [{ id: 'nested-folder', parentId: 'folder-2', name: '深层目录', kind: 'folder', modifiedAt: '2025-06-07 08:09:10' }] }
      : { id: '0', items: [{ id: 'folder-1', parentId: '0', name: '归档', kind: 'folder', modifiedAt: '2025-06-07 08:09:10' }, { id: 'file-1', parentId: '0', name: '报告.pdf', kind: 'file', size: 2_000_000, modifiedAt: '2025-06-07 08:09:10' }] })),
    createCloudDirectory: vi.fn().mockResolvedValue(undefined),
    pickAndUploadFiles: vi.fn().mockResolvedValue({ uploaded: [], failures: [] }),
    pickAndUploadDirectory: vi.fn().mockResolvedValue({ uploaded: [], failures: [] }),
    uploadCloudFiles: vi.fn().mockResolvedValue({ uploaded: [], failures: [] }),
    listLocalUploadEntries: vi.fn().mockResolvedValue({ path: 'C:/', parentPath: null, roots: [], entries: [
      { path: 'C:/a.txt', name: 'a.txt', kind: 'file' }, { path: 'C:/b.txt', name: 'b.txt', kind: 'file' }
    ] }),
    downloadCloudFile: vi.fn().mockResolvedValue('https://download.example/file'),
    downloadCloudDirectory: vi.fn().mockResolvedValue({ task: { id: 'folder-download-empty', name: '空目录', kind: 'folder', status: 'queued', progress: 0 }, queuedCount: 0, failures: [] }),
    renameCloudFile: vi.fn().mockResolvedValue(undefined),
    moveCloudFiles: vi.fn().mockResolvedValue(undefined),
    copyCloudFiles: vi.fn().mockResolvedValue(undefined),
    trashCloudFiles: vi.fn().mockResolvedValue(undefined),
    deleteCloudFiles: vi.fn().mockResolvedValue(undefined),
    restoreCloudFiles: vi.fn().mockResolvedValue(undefined),
    createCloudShare: vi.fn().mockResolvedValue({ id: 'share-1', url: 'https://pan.example/s/share-1', name: '报告.pdf', kind: 'file', sourceFileIds: ['file-1'], createdAt: '2026-09-17 10:00:00' }),
    listCloudShares: vi.fn().mockResolvedValue([]),
    cancelCloudShares: vi.fn().mockResolvedValue(undefined),
    logoutCloud: vi.fn().mockResolvedValue(undefined),
    getCloudAuthorizationCapabilities: vi.fn().mockImplementation((provider: string) => Promise.resolve({
      provider,
      web: true,
      password: provider === 'pikpak' || provider === 'baidu' || provider === 'tianyi',
      passwordLoginMode: provider === 'pikpak' || provider === 'baidu' || provider === 'tianyi'
        ? 'rustDirect'
        : provider === 'aliyun' || provider === 'xunlei' || provider === 'uc'
          ? 'officialWebView'
          : 'none'
    })),
    ...overrides
  };
}

it('replaces file details with a sharing dialog that submits the selected files and options', async () => {
  const user = userEvent.setup();
  const createCloudShare = vi.fn().mockResolvedValue({ id: 'share-7', url: 'https://pan.example/s/share-7', name: '报告.pdf', kind: 'file', sourceFileIds: ['file-1'], passCode: 'a7b2', createdAt: '2026-09-17 10:00:00' });
  const client = createCloudFake({ createCloudShare });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  const file = await screen.findByText('报告.pdf');

  fireEvent.contextMenu(file);
  await user.click(screen.getByRole('menuitem', { name: '分享' }));
  const dialog = await screen.findByRole('dialog', { name: '创建分享链接' });
  await user.type(within(dialog).getByRole('textbox', { name: '提取码' }), 'a7b2');
  await user.selectOptions(within(dialog).getByRole('combobox', { name: '有效期' }), '7');
  await user.click(within(dialog).getByRole('button', { name: '创建分享链接' }));

  expect(createCloudShare).toHaveBeenCalledWith('quark', ['file-1'], { name: '报告.pdf', passCode: 'a7b2', expiresInDays: 7 });
  expect(within(await screen.findByRole('dialog', { name: '分享链接已创建' })).getByRole('textbox', { name: '分享链接' })).toHaveValue('https://pan.example/s/share-7');
  expect(screen.queryByRole('menuitem', { name: '文件详情' })).not.toBeInTheDocument();
});

it('uses a share-specific vertical dialog layout', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="uc" client={client} initialAccount={{ id: 'uc', displayName: 'alice', provider: 'uc', authMethod: 'web' }} restoreOnMount={false} />);

  fireEvent.contextMenu(await screen.findByText('报告.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '分享' }));

  const dialog = await screen.findByRole('dialog', { name: '创建分享链接' });
  expect(dialog).toHaveClass('cloud-share-dialog');
  expect(dialog).not.toHaveClass('cloud-trash-confirm');
});

it('shows the Aliyun optional 64-character share-code requirement from its descriptor', async () => {
  const user = userEvent.setup();
  const descriptor = {
    ...cloudCatalog.find(item => item.provider === 'aliyun')!,
    sharePassCodeConstraint: {
      minLength: 1,
      maxLength: 64,
      characterSet: 'any' as const,
      hint: '提取码可留空；填写时最多 64 个字符',
    },
  };
  render(<NativeCloudManager descriptor={descriptor} provider="aliyun" client={createCloudFake()} initialAccount={{ id: 'a', displayName: 'alice', provider: 'aliyun', authMethod: 'web' }} restoreOnMount={false} />);

  fireEvent.contextMenu(await screen.findByText('报告.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '分享' }));
  const dialog = await screen.findByRole('dialog', { name: '创建分享链接' });
  expect(dialog).toHaveTextContent('提取码可留空；填写时最多 64 个字符');
  const passCode = within(dialog).getByRole('textbox', { name: '提取码' });
  expect(passCode).toHaveAttribute('maxlength', '64');
  await user.type(passCode, 'a'.repeat(65));
  expect(passCode).toHaveValue('a'.repeat(64));
});

function shareCodeDescriptor(provider: CloudProvider = 'pikpak', enabled = true) {
  const base = cloudCatalog.find(item => item.provider === provider)!;
  return { ...base, capabilities: { ...base.capabilities, shareUpdatePassCode: enabled }, sharePassCodeConstraint: { minLength: 4, maxLength: 4, characterSet: 'asciiAlphanumeric' as const, hint: '提取码必须为 4 位字母或数字' } };
}

it('creates PikPak with a server-generated code then validates and saves a custom code', async () => {
  const user = userEvent.setup();
  const updateCloudSharePassCode = vi.fn().mockResolvedValue(undefined);
  const client = createCloudFake({ updateCloudSharePassCode, createCloudShare: vi.fn().mockResolvedValue({ id: 'share-p', url: 'https://pan.example/s/share-p', name: '报告.pdf', kind: 'file', sourceFileIds: ['file-1'], passCode: 'old1' }) });
  render(<NativeCloudManager descriptor={shareCodeDescriptor()} provider="pikpak" client={client} initialAccount={{ id: 'p', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  fireEvent.contextMenu(await screen.findByText('报告.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '分享' }));
  const create = await screen.findByRole('dialog', { name: '创建分享链接' });
  expect(within(create).queryByRole('textbox', { name: '提取码' })).not.toBeInTheDocument();
  const autoGenerate = within(create).getByRole('checkbox', { name: '自动生成' });
  const codeField = within(create).getByText('提取码').parentElement!;
  expect(codeField).toHaveClass('cloud-share-code-field');
  expect(codeField.parentElement).toHaveClass('cloud-share-options');
  expect(codeField.parentElement).toContainElement(within(create).getByText('有效期'));
  expect(autoGenerate.closest('label')).toHaveClass('cloud-share-checkbox');
  expect(autoGenerate.closest('label')).toContainElement(within(create).getByText('自动生成'));
  expect(within(create).queryByText('自动生成提取码')).not.toBeInTheDocument();
  await user.click(autoGenerate);
  await user.click(within(create).getByRole('button', { name: '创建分享链接' }));
  expect(client.createCloudShare).toHaveBeenCalledWith('pikpak', ['file-1'], { name: '报告.pdf', generatePassCode: true });
  const result = await screen.findByRole('dialog', { name: '分享链接已创建' });
  expect(result.querySelector('.cloud-share-passcode-editor')).toBeTruthy();
  const code = within(result).getByRole('textbox', { name: '新提取码' });
  expect(code).toHaveValue('old1');
  await user.clear(code);
  await user.type(code, 'ab!');
  expect(within(result).getByRole('button', { name: '保存提取码' })).toBeDisabled();
  expect(within(result).getByRole('alert')).toHaveTextContent('提取码必须为 4 位字母或数字');
  await user.clear(code);
  await user.type(code, 'a1B2');
  await user.click(within(result).getByRole('button', { name: '保存提取码' }));
  await within(result).findByText('提取码已更新');
  expect(updateCloudSharePassCode).toHaveBeenCalledWith('pikpak', 'share-p', 'a1B2');
  expect(within(result).getByText('a1B2', { selector: 'b' })).toBeVisible();
  expect(client.createCloudShare).toHaveBeenCalledTimes(1);
});

it('keeps the PikPak share-code input, save button, and validation message in separate grid cells', () => {
  const styles = readFileSync('src/styles.css', 'utf8');
  expect(styles).toContain('.cloud-share-dialog .cloud-share-passcode-editor{display:grid;grid-template-columns:minmax(0,1fr) auto;align-items:end}');
  expect(styles).toContain('.cloud-share-dialog .cloud-share-passcode-editor button{min-width:92px;margin:0;white-space:nowrap}');
  expect(styles).toContain('.cloud-share-dialog .cloud-share-passcode-editor p{grid-column:1/-1;margin:0;font-size:11px}');
});

it('preserves the old share code on failure and reopens my shares with the successfully updated code', async () => {
  const user = userEvent.setup();
  const updateCloudSharePassCode = vi.fn().mockRejectedValueOnce(new Error('服务器拒绝')).mockResolvedValue(undefined);
  const client = createCloudFake({ updateCloudSharePassCode, listCloudShares: vi.fn().mockResolvedValue([{ id: 'share-p', url: 'https://pan.example/s/share-p', name: '已有分享.pdf', kind: 'file', sourceFileIds: ['file-1'], passCode: 'old1' }]) });
  render(<NativeCloudManager descriptor={shareCodeDescriptor()} provider="pikpak" client={client} initialAccount={{ id: 'p', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  await user.dblClick(await screen.findByText('我的分享'));
  fireEvent.contextMenu(await screen.findByText('已有分享.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '修改提取码' }));
  let result = await screen.findByRole('dialog', { name: '分享链接已创建' });
  const code = within(result).getByRole('textbox', { name: '新提取码' });
  await user.clear(code);
  await user.type(code, 'new2');
  await user.click(within(result).getByRole('button', { name: '保存提取码' }));
  expect(await within(result).findByRole('alert')).toHaveTextContent('修改提取码失败：服务器拒绝');
  expect(within(result).getByText('old1', { selector: 'b' })).toBeVisible();
  await user.click(within(result).getByRole('button', { name: '保存提取码' }));
  await within(result).findByText('提取码已更新');
  await user.click(within(result).getByRole('button', { name: '完成' }));
  await user.click(screen.getByRole('button', { name: '修改提取码' }));
  result = await screen.findByRole('dialog', { name: '分享链接已创建' });
  expect(within(result).getByRole('textbox', { name: '新提取码' })).toHaveValue('new2');
  expect(within(result).getByText('new2', { selector: 'b' })).toBeVisible();
});

it('does not expose share-code updates when the provider capability is absent', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudShares: vi.fn().mockResolvedValue([{ id: 'share-q', url: 'https://pan.example/s/share-q', name: '已有分享.pdf', kind: 'file', sourceFileIds: ['file-1'], passCode: 'old1' }]) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  await user.dblClick(await screen.findByText('我的分享'));
  fireEvent.contextMenu(await screen.findByText('已有分享.pdf'));
  expect(screen.queryByRole('button', { name: '修改提取码' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '修改提取码' })).not.toBeInTheDocument();
});

it('keeps share-code editing pending until the server confirms and allows adding a code', async () => {
  const user = userEvent.setup();
  let resolveUpdate!: () => void;
  const updateCloudSharePassCode = vi.fn(() => new Promise<void>(resolve => { resolveUpdate = resolve; }));
  const client = createCloudFake({ updateCloudSharePassCode, listCloudShares: vi.fn().mockResolvedValue([{ id: 'share-p', url: 'https://pan.example/s/share-p', name: '无提取码.pdf', kind: 'file', sourceFileIds: ['file-1'] }]) });
  render(<NativeCloudManager descriptor={shareCodeDescriptor()} provider="pikpak" client={client} initialAccount={{ id: 'p', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  await user.dblClick(await screen.findByText('我的分享'));
  await user.click(await screen.findByText('无提取码.pdf'));
  await user.click(screen.getByRole('button', { name: '修改提取码' }));
  const result = await screen.findByRole('dialog', { name: '分享链接已创建' });
  const code = within(result).getByRole('textbox', { name: '新提取码' });
  expect(within(result).getByRole('button', { name: '保存提取码' })).toBeDisabled();
  await user.type(code, 'a1B2');
  await user.click(within(result).getByRole('button', { name: '保存提取码' }));
  expect(within(result).getByRole('button', { name: '正在保存…' })).toBeDisabled();
  expect(within(result).getByRole('button', { name: '完成' })).toBeDisabled();
  expect(code).toBeDisabled();
  await user.keyboard('{Escape}');
  expect(result).toBeVisible();
  expect(within(result).queryByText('a1B2', { selector: 'b' })).not.toBeInTheDocument();
  await act(async () => resolveUpdate());
  expect(await within(result).findByText('a1B2', { selector: 'b' })).toBeVisible();
  expect(updateCloudSharePassCode).toHaveBeenCalledTimes(1);
});

it('uses a provider-declared UC share-code constraint while the user types', async () => {
  // Break caught: removing the shared dialog's constraint handling would let
  // an invalid UC code reach the provider as a generic share-create failure.
  const user = userEvent.setup();
  const createCloudShare = vi.fn().mockResolvedValue({ id: 'share-uc', url: 'https://drive.uc.cn/s/share-uc', name: '报告.pdf', kind: 'file', sourceFileIds: ['file-1'] });
  const descriptor = {
    ...cloudCatalog.find(item => item.provider === 'uc')!,
    sharePassCodeConstraint: {
      minLength: 4,
      maxLength: 4,
      characterSet: 'asciiAlphanumeric' as const,
      hint: '提取码必须为 4 位字母或数字',
    },
  };
  render(<NativeCloudManager descriptor={descriptor} provider="uc" client={createCloudFake({ createCloudShare })} initialAccount={{ id: 'uc', displayName: 'alice', provider: 'uc', authMethod: 'web' }} restoreOnMount={false} />);

  fireEvent.contextMenu(await screen.findByText('报告.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '分享' }));
  const dialog = await screen.findByRole('dialog', { name: '创建分享链接' });
  const passCode = within(dialog).getByRole('textbox', { name: '提取码' });
  expect(within(dialog).getByText('将分享 报告.pdf。可选设置提取码和有效期，提取码必须为 4 位字母或数字')).toBeVisible();

  await user.type(passCode, 'a7!');
  expect(within(dialog).queryByText('UC 提取码需为 4 位字母或数字')).not.toBeInTheDocument();
  expect(within(dialog).getByRole('button', { name: '创建分享链接' })).toBeDisabled();

  await user.clear(passCode);
  await user.type(passCode, 'a7b2');
  await user.click(within(dialog).getByRole('button', { name: '创建分享链接' }));
  expect(createCloudShare).toHaveBeenCalledWith('uc', ['file-1'], { name: '报告.pdf', passCode: 'a7b2' });
});

it('requires a Baidu code or explicit automatic generation and copies the resulting code', async () => {
  const user = userEvent.setup();
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
  const createCloudShare = vi.fn().mockResolvedValue({ id: 'bd-share', url: 'https://pan.baidu.com/s/1created', name: '报告.pdf', kind: 'file', sourceFileIds: ['file-1'], passCode: 'a7B2' });
  render(<CloudManager provider="baidu" client={createCloudFake({ createCloudShare })} initialAccount={{ id: 'bd', displayName: 'alice', provider: 'baidu', authMethod: 'web' }} restoreOnMount={false} />);
  fireEvent.contextMenu(await screen.findByText('报告.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '分享' }));
  const dialog = await screen.findByRole('dialog', { name: '创建分享链接' });
  expect(within(dialog).queryByText('提取码（可选）')).not.toBeInTheDocument();
  const autoGenerate = within(dialog).getByRole('checkbox', { name: '自动生成' });
  expect(autoGenerate).toBeChecked();
  await user.click(autoGenerate);
  expect(within(dialog).getByRole('button', { name: '创建分享链接' })).toBeDisabled();
  await user.type(within(dialog).getByRole('textbox', { name: '提取码' }), 'a7B2');
  await user.click(within(dialog).getByRole('button', { name: '创建分享链接' }));
  expect(createCloudShare).toHaveBeenCalledWith('baidu', ['file-1'], { name: '报告.pdf', passCode: 'a7B2' });
  const result = await screen.findByRole('dialog', { name: '分享链接已创建' });
  expect(within(result).getByRole('textbox', { name: '分享链接' })).toHaveValue('https://pan.baidu.com/s/1created?pwd=a7B2');
  await user.click(within(result).getByRole('button', { name: '复制分享链接' }));
  await waitFor(() => expect(writeText).toHaveBeenCalledWith('https://pan.baidu.com/s/1created?pwd=a7B2'));
});

it('uses the same four-character alphanumeric share-code guidance for Quark', async () => {
  const user = userEvent.setup();
  const createCloudShare = vi.fn().mockResolvedValue({ id: 'share-quark', url: 'https://pan.quark.cn/s/share-quark', name: '报告.pdf', kind: 'file', sourceFileIds: ['file-1'] });
  render(<CloudManager provider="quark" client={createCloudFake({ createCloudShare })} initialAccount={{ id: 'quark', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  fireEvent.contextMenu(await screen.findByText('报告.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '分享' }));
  const dialog = await screen.findByRole('dialog', { name: '创建分享链接' });
  const passCode = within(dialog).getByRole('textbox', { name: '提取码' });
  expect(within(dialog).getByText('将分享 报告.pdf。可选设置提取码和有效期，提取码必须为 4 位字母或数字')).toBeVisible();
  expect(passCode).toHaveAttribute('maxlength', '4');

  await user.type(passCode, 'a7!');
  expect(within(dialog).getByRole('button', { name: '创建分享链接' })).toBeDisabled();
  await user.clear(passCode);
  await user.type(passCode, 'a7B2');
  await user.click(within(dialog).getByRole('button', { name: '创建分享链接' }));
  expect(createCloudShare).toHaveBeenCalledWith('quark', ['file-1'], { name: '报告.pdf', passCode: 'a7B2' });
});

it('refreshes my shares from the provider after cancelling a share', async () => {
  // Break caught: skipping the follow-up list leaves the virtual directory
  // disconnected from the server state after a successful cancellation.
  const user = userEvent.setup();
  const cancelCloudShares = vi.fn().mockResolvedValue(undefined);
  const listCloudShares = vi.fn()
    .mockResolvedValueOnce([{ id: 'share-9', url: 'https://pan.example/s/share-9', name: '共享目录', kind: 'folder', sourceFileIds: ['folder-1'], createdAt: '2026-09-17 10:00:00' }])
    .mockResolvedValueOnce([]);
  const client = createCloudFake({ listCloudShares, cancelCloudShares });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await user.dblClick(await screen.findByText('我的分享'));
  expect(await screen.findByText('共享目录')).toBeVisible();
  expect(listCloudShares).toHaveBeenCalledWith('quark');
  fireEvent.contextMenu(screen.getByText('共享目录'));
  expect(screen.getByRole('menuitem', { name: '复制分享链接' })).toBeVisible();
  await user.click(screen.getByRole('menuitem', { name: '取消分享' }));
  await waitFor(() => expect(cancelCloudShares).toHaveBeenCalledWith('quark', ['share-9']));
  await waitFor(() => expect(screen.queryByText('共享目录')).not.toBeInTheDocument());
  expect(screen.getByText('暂无分享文件')).toBeVisible();
  expect(listCloudShares).toHaveBeenCalledTimes(2);
});

it('keeps my-shares visually distinct from a same-named folder and read-only', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudDirectory: vi.fn().mockResolvedValue({
    id: '0',
    items: [{ id: 'ordinary-my-shares', parentId: '0', name: '我的分享', kind: 'folder' }]
  }) });
  const { container } = render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await waitFor(() => expect(container.querySelector('[data-cloud-virtual-folder="my-shares"]')).toBeTruthy());
  const virtualFolder = container.querySelector<HTMLElement>('[data-cloud-virtual-folder="my-shares"]');
  expect(virtualFolder).toBeTruthy();
  expect(virtualFolder?.querySelector('i.my-shares')).toBeTruthy();
  expect(container.querySelector('[data-cloud-file-row]:not([data-cloud-virtual-folder]) i.folder')).toBeTruthy();

  await user.dblClick(virtualFolder!);
  await screen.findByText('暂无分享文件');
  expect(screen.queryByRole('button', { name: '上传', exact: true })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '新建目录', exact: true })).not.toBeInTheDocument();
});

it('shows loading then limited capacity on an authorized account card', async () => {
  let resolveCapacity!: (value: any) => void;
  const client = createCloudFake({
    getCloudAccountCapacity: vi.fn(() => new Promise(resolve => { resolveCapacity = resolve; }))
  });
  render(<CloudManager provider="quark" client={client} authorizationOnly initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  expect(screen.getByText('容量获取中…')).toBeVisible();
  await act(async () => { resolveCapacity({ provider: 'quark', accountId: 'q-1', capacity: { kind: 'limited', totalBytes: 100 * 1024 ** 3, usedBytes: 12 * 1024 ** 3 }, updatedAt: 1 }); });
  expect(await screen.findByText('已使用 12.0 GB / 100.0 GB')).toBeVisible();
});

it('hides account capacity while the provider information view is open', async () => {
  const client = createCloudFake();
  const user = userEvent.setup();
  render(<CloudManager provider="quark" client={client} authorizationOnly initialAccount={{ id: 'quark-account', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  expect(await screen.findByText('已使用 12.0 GB / 100.0 GB')).toBeVisible();

  await user.click(screen.getByRole('button', { name: '查看夸克网盘信息' }));

  expect(screen.getByRole('region', { name: '夸克网盘信息' })).toBeVisible();
  expect(screen.queryByText('已使用 12.0 GB / 100.0 GB')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '关闭夸克网盘信息' }));
  expect(screen.getByText('已使用 12.0 GB / 100.0 GB')).toBeVisible();
});

it('keeps the account authorized when the first capacity request fails', async () => {
  const client = createCloudFake({ getCloudAccountCapacity: vi.fn().mockRejectedValue(new Error('offline')) });
  render(<CloudManager provider="quark" client={client} authorizationOnly initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
  expect(await screen.findByText('容量暂不可用')).toBeVisible();
  expect(screen.getByRole('button', { name: '已经完成授权' })).toBeVisible();
});

it('retains capacity and marks an event-driven refresh failure', async () => {
  vi.useFakeTimers();
  try {
    const client = createCloudFake({ getCloudAccountCapacity: vi.fn()
      .mockResolvedValueOnce({ provider: 'quark', accountId: 'q-1', capacity: { kind: 'unlimited', usedBytes: 3 * 1024 ** 3 }, updatedAt: 1 })
      .mockRejectedValueOnce(new Error('offline')) });
    render(<CloudManager provider="quark" client={client} authorizationOnly initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
    await act(async () => {});
    expect(screen.getByText('已使用 3.0 GB / 不限容量')).toBeVisible();
    await act(async () => {
      client.emitCapacityInvalidated({ provider: 'quark', accountId: 'q-1', reason: 'delete' });
      await vi.advanceTimersByTimeAsync(400);
    });
    expect(screen.getByText(/已使用 3.0 GB \/ 不限容量/)).toHaveTextContent('更新失败');
    expect(client.getCloudAccountCapacity).toHaveBeenCalledTimes(2);
  } finally {
    vi.useRealTimers();
  }
});

it.each(['quark', 'pikpak', 'baidu', 'tianyi', 'aliyun', 'xunlei', 'uc'] as const)(
  'keeps %s file controls compact and the refresh icon centered', async (provider) => {
    const stylesheet = document.createElement('style');
    stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
    document.head.appendChild(stylesheet);
    try {
      const client = createCloudFake();
      const { container } = render(<CloudManager provider={provider} client={client}
        initialAccount={{ id: `${provider}-account`, provider, displayName: 'layout-test', authMethod: 'web' }}
        restoreOnMount={false} showSessionActions={false} />);
      await screen.findByRole('tree');
      const manager = container.querySelector('.cloud-file-manager')!;
      const list = container.querySelector('.cloud-list')!;
      expect(getComputedStyle(manager).display).toBe('flex');
      expect(getComputedStyle(manager).minHeight).toBe('0');
      expect(getComputedStyle(list).flexGrow).toBe('1');
      expect(getComputedStyle(list).minHeight).toBe('0');
      expect(getComputedStyle(list).overflowX).toBe('hidden');
      const refresh = screen.getByRole('button', { name: '刷新' });
      const breadcrumbs = screen.getByRole('navigation', { name: /^当前目录：/ });
      expect(refresh.compareDocumentPosition(breadcrumbs) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
      expect(getComputedStyle(refresh).display).toBe('grid');
      expect(getComputedStyle(refresh).placeItems).toBe('center');
      expect(getComputedStyle(refresh).padding).toBe('0px');
    } finally {
      stylesheet.remove();
    }
  },
);

it('submits provided credentials once while automatic password login is pending', async () => {
  let finish!: (value: { id: string; provider: 'baidu'; displayName: string; authMethod: 'password' }) => void;
  const pending = new Promise<{ id: string; provider: 'baidu'; displayName: string; authMethod: 'password' }>(resolve => { finish = resolve; });
  const client = createCloudFake({ loginCloudWithPassword: vi.fn().mockReturnValue(pending) });
  const user = userEvent.setup();
  render(<CloudManager provider="baidu" client={client} authorizationOnly />);
  await user.type(screen.getByLabelText('账号'), 'provided-user');
  await user.type(screen.getByLabelText('密码'), 'provided-password');
  await user.click(screen.getByRole('button', { name: '登录' }));
  expect(screen.getByRole('button', { name: '正在自动登录…' })).toBeDisabled();
  expect(screen.getByRole('button', { name: '使用网页授权' })).toBeDisabled();
  expect(client.loginCloudWithPassword).toHaveBeenCalledWith('baidu', { username: 'provided-user', password: 'provided-password' });
  fireEvent.submit(screen.getByLabelText('密码').closest('form')!);
  expect(client.loginCloudWithPassword).toHaveBeenCalledTimes(1);
  await act(async () => { finish({ id: 'account-1', provider: 'baidu', displayName: 'alice', authMethod: 'password' }); });
  expect(screen.queryByRole('button', { name: '正在自动登录…' })).not.toBeInTheDocument();
});

it('submits Xunlei credentials through the official WebView path and retains webpage authorization', async () => {
  const client = createCloudFake();
  const user = userEvent.setup();
  render(<CloudManager provider="xunlei" client={client} authorizationOnly />);

  expect(screen.getByRole('button', { name: '使用网页授权' })).toBeVisible();
  await user.type(screen.getByLabelText('账号'), 'xunlei-user');
  await user.type(screen.getByLabelText('密码'), 'xunlei-password');
  await user.click(screen.getByRole('button', { name: '登录' }));

  expect(client.loginCloudWithPassword).toHaveBeenCalledWith('xunlei', {
    username: 'xunlei-user', password: 'xunlei-password'
  });
  expect(screen.queryByLabelText('账号')).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: '已经登录' })).toBeVisible();
});

it('refills saved credentials after manual logout and logs in only when clicked', async () => {
  const client = createCloudFake({ getSavedCloudCredentials: vi.fn().mockResolvedValue({ username: 'saved-user', hasSavedPassword: true }) });
  const user = userEvent.setup();
  render(<CloudManager provider="tianyi" client={client} authorizationOnly initialAccount={{ id: 'a', displayName: 'alice', provider: 'tianyi', authMethod: 'password' }} restoreOnMount={false} />);
  await user.click(screen.getByRole('button', { name: '退出登录' }));
  await user.click(screen.getByRole('button', { name: '确认退出登录' }));
  await waitFor(() => expect(screen.getByLabelText('账号')).toHaveValue('saved-user'));
  expect(screen.getByLabelText('密码')).toHaveValue('');
  expect(screen.getByLabelText('密码')).toHaveAttribute('placeholder', '******');
  expect(client.loginCloudWithPassword).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: '登录' }));
  expect(client.loginCloudWithPassword).toHaveBeenCalledWith('tianyi', { username: 'saved-user', useSavedPassword: true });
});

it('refills the newly saved account after successful replacement and a second logout', async () => {
  let savedUsername = 'alice';
  const client = createCloudFake({
    getSavedCloudCredentials: vi.fn(async () => ({ username: savedUsername, hasSavedPassword: true })),
    loginCloudWithPassword: vi.fn(async (_provider, credentials) => {
      savedUsername = credentials.username;
      return { id: 'b', displayName: 'bob', provider: 'tianyi', authMethod: 'password' };
    })
  });
  const user = userEvent.setup();
  render(<CloudManager provider="tianyi" client={client} authorizationOnly />);
  await waitFor(() => expect(screen.getByLabelText('账号')).toHaveValue('alice'));
  fireEvent.change(screen.getByLabelText('账号'), { target: { value: 'bob' } });
  fireEvent.change(screen.getByLabelText('密码'), { target: { value: 'new-secret' } });
  await user.click(screen.getByRole('button', { name: '登录' }));
  await user.click(await screen.findByRole('button', { name: '退出登录' }));
  await user.click(screen.getByRole('button', { name: '确认退出登录' }));
  await waitFor(() => expect(screen.getByLabelText('账号')).toHaveValue('bob'));
  expect(screen.getByLabelText('密码')).toHaveAttribute('placeholder', '******');
  expect(screen.getByLabelText('密码')).toHaveValue('');
});

it('restores encrypted login input into masked fields and keeps it after a failed attempt', async () => {
  const client = createCloudFake({
    getSavedCloudCredentials: vi.fn().mockResolvedValue({ username: 'saved-user', hasSavedPassword: true }),
    loginCloudWithPassword: vi.fn().mockRejectedValue(Object.assign(new Error('登录失败'), { code: 'provider_error' }))
  });
  const user = userEvent.setup();
  render(<CloudManager provider="tianyi" client={client} authorizationOnly />);

  expect(await screen.findByLabelText('账号')).toHaveValue('saved-user');
  const passwordInput = screen.getByLabelText('密码');
  expect(passwordInput).toHaveAttribute('type', 'password');
  expect(passwordInput).toHaveValue('');
  expect(passwordInput).toHaveAttribute('placeholder', '******');
  await user.click(screen.getByRole('button', { name: '登录' }));
  await screen.findByText('登录失败');
  expect(screen.getByLabelText('账号')).toHaveValue('saved-user');
  expect(client.loginCloudWithPassword).toHaveBeenCalledWith('tianyi', { username: 'saved-user', useSavedPassword: true });
  expect(screen.getByLabelText('密码')).toHaveAttribute('placeholder', '******');
});

it('preserves a password edited while an earlier login is pending and submits the newer value next', async () => {
  let rejectFirst!: (error: unknown) => void;
  const pending = new Promise<never>((_, reject) => { rejectFirst = reject; });
  const client = createCloudFake({
    loginCloudWithPassword: vi.fn().mockReturnValueOnce(pending).mockRejectedValue(new Error('第二次失败')),
    getSavedCloudCredentials: vi.fn().mockResolvedValueOnce(null).mockResolvedValue({ username: 'alice', hasSavedPassword: true })
  });
  render(<CloudManager provider="baidu" client={client} authorizationOnly />);
  await act(async () => { await Promise.resolve(); });
  fireEvent.change(screen.getByLabelText('账号'), { target: { value: 'alice' } });
  fireEvent.change(screen.getByLabelText('密码'), { target: { value: 'P1' } });
  fireEvent.submit(screen.getByLabelText('密码').closest('form')!);
  expect(screen.getByLabelText('密码')).not.toBeDisabled();
  fireEvent.change(screen.getByLabelText('密码'), { target: { value: 'P2' } });
  await act(async () => { rejectFirst(new Error('第一次失败')); });
  expect(screen.getByLabelText('密码')).toHaveValue('P2');
  expect(screen.getByLabelText('密码')).toHaveAttribute('placeholder', '密码');
  await act(async () => { fireEvent.submit(screen.getByLabelText('密码').closest('form')!); });
  expect(client.loginCloudWithPassword).toHaveBeenLastCalledWith('baidu', { username: 'alice', password: 'P2' });
});

it('does not restore a stale saved-password placeholder after editing during metadata refresh', async () => {
  let finishMetadata!: (value: { username: string; hasSavedPassword: boolean }) => void;
  const metadata = new Promise<{ username: string; hasSavedPassword: boolean }>(resolve => { finishMetadata = resolve; });
  const client = createCloudFake({
    loginCloudWithPassword: vi.fn().mockRejectedValue(new Error('登录失败')),
    getSavedCloudCredentials: vi.fn().mockResolvedValueOnce(null).mockReturnValueOnce(metadata)
  });
  render(<CloudManager provider="baidu" client={client} authorizationOnly />);
  await act(async () => { await Promise.resolve(); });
  fireEvent.change(screen.getByLabelText('账号'), { target: { value: 'alice' } });
  fireEvent.change(screen.getByLabelText('密码'), { target: { value: 'P1' } });
  await act(async () => { fireEvent.submit(screen.getByLabelText('密码').closest('form')!); });
  fireEvent.change(screen.getByLabelText('密码'), { target: { value: 'P2' } });
  fireEvent.change(screen.getByLabelText('密码'), { target: { value: '' } });
  await act(async () => { finishMetadata({ username: 'alice', hasSavedPassword: true }); });
  expect(screen.getByLabelText('密码')).toHaveValue('');
  expect(screen.getByLabelText('密码')).toHaveAttribute('placeholder', '密码');
  fireEvent.submit(screen.getByLabelText('密码').closest('form')!);
  expect(screen.getByText('请输入账号和密码后再登录。')).toBeVisible();
  expect(client.loginCloudWithPassword).toHaveBeenCalledTimes(1);
});

it('sends only the edited password and never the saved-password handle', async () => {
  const client = createCloudFake({ getSavedCloudCredentials: vi.fn().mockResolvedValue({ username: 'saved-user', hasSavedPassword: true }) });
  render(<CloudManager provider="tianyi" client={client} authorizationOnly />);
  await waitFor(() => expect(screen.getByLabelText('账号')).toHaveValue('saved-user'));
  fireEvent.change(screen.getByLabelText('密码'), { target: { value: 'new-secret' } });
  await act(async () => { fireEvent.submit(screen.getByLabelText('密码').closest('form')!); });
  expect(client.loginCloudWithPassword).toHaveBeenCalledWith('tianyi', { username: 'saved-user', password: 'new-secret' });
});

it('never creates account switching tabs inside one provider manager', async () => {
  const client = createCloudFake({ getCloudAccount: vi.fn().mockResolvedValue({ id: 'only', displayName: 'alice', provider: 'quark', authMethod: 'web' }) });
  render(<CloudManager provider="quark" client={client} />);
  await screen.findByRole('tree', { name: '夸克网盘文件' });
  expect(screen.queryByRole('tablist', { name: '夸克账号' })).not.toBeInTheDocument();
});

it('keeps the unauthenticated provider card focused on the available sign-in action', async () => {
  render(<CloudManager provider="quark" client={createCloudFake()} />);

  expect(screen.getByRole('region', { name: '夸克网盘账号授权' })).toBeVisible();
  expect(screen.queryByText('自有网盘 · 本地会话')).not.toBeInTheDocument();
  expect(screen.queryByText(/授权完成前不会显示或加载任何文件/)).not.toBeInTheDocument();
  expect(screen.queryByText(/夸克未提供可用的账号密码开放登录接口/)).not.toBeInTheDocument();
  expect(screen.queryByLabelText('夸克账号')).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '登录夸克账号' })).not.toBeInTheDocument();
  expect(await screen.findByRole('button', { name: '使用网页授权' })).toBeVisible();
  expect(screen.queryByRole('status')).not.toBeInTheDocument();
  expect(screen.queryByRole('tree', { name: '夸克网盘文件' })).not.toBeInTheDocument();
  expect(screen.queryByText('下载示例.txt')).not.toBeInTheDocument();
});

it.each([
  ['quark', '夸克网盘', false], ['pikpak', 'PikPak 网盘', true], ['baidu', '百度网盘', true], ['tianyi', '天翼云盘', true],
  ['aliyun', '阿里云盘', false], ['xunlei', '迅雷网盘', true], ['uc', 'UC 网盘', false]
] as const)('keeps the %s login card concise while prioritizing its supported login method', async (provider, label, supportsPasswordLogin) => {
  const client = createCloudFake({ getCloudAuthorizationCapabilities: vi.fn().mockReturnValue(new Promise<never>(() => undefined)) });
  render(<CloudManager provider={provider} client={client} authorizationOnly />);

  expect(await screen.findByRole('button', { name: '使用网页授权' })).toBeVisible();
  expect(screen.queryByLabelText(`${label}账号`)).not.toBeInTheDocument();
  expect(screen.queryByLabelText(`${label}密码`)).not.toBeInTheDocument();
  expect(screen.queryByText('账号和密码仅加密保存在本机，用于登录失效后的自动恢复。')).not.toBeInTheDocument();
  expect(screen.queryByText('请在官方网页中使用账号密码登录；应用不会收集或保存密码。')).not.toBeInTheDocument();
  if (supportsPasswordLogin) {
    expect(screen.getByLabelText('账号')).toBeVisible();
    expect(screen.getByLabelText('密码')).toBeVisible();
    expect(screen.getByRole('button', { name: '登录' })).toHaveClass('primary');
  } else {
    expect(screen.queryByLabelText('账号')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('密码')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '登录' })).not.toBeInTheDocument();
  }
  expect(client.getCloudAuthorizationCapabilities).not.toHaveBeenCalled();
});

it.each([
  ['quark', '夸克网盘', 'https://pan.quark.cn'],
  ['pikpak', 'PikPak 网盘', 'https://mypikpak.com/drive'],
  ['baidu', '百度网盘', 'https://pan.baidu.com'],
  ['tianyi', '天翼云盘', 'https://cloud.189.cn'],
  ['aliyun', '阿里云盘', 'https://www.aliyundrive.com/drive'],
  ['xunlei', '迅雷网盘', 'https://pan.xunlei.com'],
  ['uc', 'UC 网盘', 'https://drive.uc.cn']
] as const)('shows the %s provider information inside its authorization card', async (provider, label, authorizationUrl) => {
  const user = userEvent.setup();
  const descriptor = { ...cloudCatalog.find(item => item.provider === provider)!, authorizationUrl };
  render(<NativeCloudManager descriptor={descriptor} provider={provider} client={createCloudFake()} authorizationOnly />);
  const card = screen.getByRole('region', { name: `${label}账号授权` });

  await user.click(screen.getByRole('button', { name: `查看${label}信息` }));

  expect(screen.queryByRole('dialog', { name: `${label}信息` })).not.toBeInTheDocument();
  const info = within(card).getByRole('region', { name: `${label}信息` });
  expect(within(info).getByRole('link', { name: `${label}官方网站` })).toHaveAttribute('href', authorizationUrl);
  expect(within(info).getByRole('link', { name: '插件制作者 aigutta' })).toHaveAttribute('href', 'https://versadown.aigutta.com');
  const close = within(card).getByRole('button', { name: `关闭${label}信息` });
  expect(close).toHaveTextContent('×');
  await user.click(close);
  expect(within(card).queryByRole('region', { name: `${label}信息` })).not.toBeInTheDocument();
  expect(within(card).getByRole('button', { name: `查看${label}信息` })).toBeVisible();
});

it('shows the PikPak regional and Xunlei SVIP notices from their information buttons', async () => {
  const user = userEvent.setup();
  const pikpak = { ...cloudCatalog.find(item => item.provider === 'pikpak')!, authorizationUrl: 'https://mypikpak.com/drive' };
  const view = render(<NativeCloudManager descriptor={pikpak} provider="pikpak" client={createCloudFake()} authorizationOnly />);
  await user.click(screen.getByRole('button', { name: '查看PikPak 网盘信息' }));
  expect(screen.getByRole('region', { name: 'PikPak 网盘信息' })).toHaveTextContent('此网盘在中国大陆可能无法直接访问。');

  const xunlei = { ...cloudCatalog.find(item => item.provider === 'xunlei')!, authorizationUrl: 'https://pan.xunlei.com' };
  view.unmount();
  const xunleiView = render(<NativeCloudManager descriptor={xunlei} provider="xunlei" client={createCloudFake()} authorizationOnly />);
  await user.click(screen.getByRole('button', { name: '查看迅雷网盘信息' }));
  expect(screen.getByRole('region', { name: '迅雷网盘信息' })).toHaveTextContent('安装迅雷官方客户端后方可使用 SVIP 下载通道。');

  xunleiView.unmount();
  render(<CloudManager provider="quark" client={createCloudFake()} authorizationOnly />);
  await user.click(screen.getByRole('button', { name: '查看夸克网盘信息' }));
  expect(screen.getByRole('region', { name: '夸克网盘信息' })).not.toHaveTextContent('中国大陆可能无法直接访问');
});

it('shows Tianyi device-lock guidance and keeps provider information available after authorization', async () => {
  const user = userEvent.setup();
  const descriptor = { ...cloudCatalog.find(item => item.provider === 'tianyi')!, authorizationUrl: 'https://cloud.189.cn' };
  render(<NativeCloudManager descriptor={descriptor} provider="tianyi" client={createCloudFake()} authorizationOnly initialAccount={{ id: 't-1', displayName: 'alice', provider: 'tianyi', authMethod: 'password' }} restoreOnMount={false} />);

  await user.click(screen.getByRole('button', { name: '查看天翼云盘信息' }));
  const info = screen.getByRole('region', { name: '天翼云盘信息' });
  expect(info).toHaveTextContent('如果通过账号和密码登录，推荐登录门户网址');
  expect(info).toHaveTextContent('然后点击“账号安全”，关闭“设备锁”，这样无需每次登录都进行短信认证。');
  expect(within(info).getByRole('link', { name: '天翼账号登录门户' })).toHaveAttribute('href', 'https://e.dlife.cn');
});

it.each([
  ['aliyun', '阿里云盘'],
  ['xunlei', '迅雷网盘'],
  ['uc', 'UC 网盘']
] as const)('keeps the account and offers official WebView recovery for %s', async (provider, label) => {
  const client = createCloudFake({ listCloudDirectory: vi.fn().mockRejectedValue({ code: 'web_auth_required', message: 'verification required' }) });
  render(<CloudManager provider={provider} client={client} initialAccount={{ id: `${provider}-account`, displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

  expect(await screen.findByRole('button', { name: '重新网页授权' })).toBeVisible();
  expect(screen.getByRole('region', { name: `${label}文件管理器` })).toBeVisible();
});

it('asks to repeat a mutation after the session was automatically restored', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({
    uploadCloudFiles: vi.fn().mockRejectedValue({
      code: 'session_renewed_retry_operation',
      message: 'this provider response must not be shown'
    })
  });
  render(<CloudManager provider="baidu" client={client} initialAccount={{ id: 'account-1', displayName: 'alice', provider: 'baidu', authMethod: 'password' }} restoreOnMount={false} />);

  await screen.findByRole('tree', { name: '百度网盘文件' });
  await user.click(screen.getByRole('button', { name: '上传' }));
  await confirmAllLocalUploads(user);

  expect(await screen.findByRole('alert')).toHaveTextContent('已自动恢复登录，请重新执行本次操作');
  expect(screen.getByRole('alert')).not.toHaveTextContent('this provider response must not be shown');
});

it('rejects a web authorization account returned for another provider', async () => {
  const user = userEvent.setup();
  const onAccountAuthorized = vi.fn();
  const client = createCloudFake({ completeCloudAuthorization: vi.fn().mockResolvedValue({ id: 'p-1', displayName: 'p-alice', provider: 'pikpak', authMethod: 'web' }) });
  render(<CloudManager provider="quark" client={client} authorizationOnly onAccountAuthorized={onAccountAuthorized} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));

  expect(await screen.findByRole('status')).toHaveTextContent('授权返回的服务商不匹配');
  expect(onAccountAuthorized).not.toHaveBeenCalled();
  expect(screen.queryByRole('button', { name: '已经完成授权' })).not.toBeInTheDocument();
});

it('recovers an existing account on mount before loading its root directory', async () => {
  const client = createCloudFake({ getCloudAccount: vi.fn().mockResolvedValue({ id: 'saved-1', displayName: 'saved-alice', provider: 'quark', authMethod: 'web' }) });
  render(<CloudManager provider="quark" client={client} />);

  expect(await screen.findByRole('region', { name: '夸克网盘文件管理器' })).toBeVisible();
  expect(client.listCloudDirectory).toHaveBeenCalledWith('quark', '0');
  expect(await screen.findByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
});

it.each([
  ['baidu', '百度网盘', '/'],
  ['tianyi', '天翼云盘', '-11'],
  ['aliyun', '阿里云盘', 'root'],
  ['xunlei', '迅雷网盘', '0']
] as const)('uses the native root identifier for %s', async (provider, label, root) => {
  const client = createCloudFake({
    getCloudAccount: vi.fn().mockResolvedValue({ id: `${provider}-account`, displayName: 'alice', provider, authMethod: 'web' })
  });
  render(<CloudManager provider={provider} client={client} />);

  expect(await screen.findByRole('tree', { name: `${label}文件` })).toBeVisible();
  expect(client.listCloudDirectory).toHaveBeenCalledWith(provider, root);
});

it('replaces the single account during renewed web authorization without retaining a previous tab', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({
    getCloudAccount: vi.fn().mockResolvedValue({ id: 'first', displayName: 'first-alice', provider: 'quark', authMethod: 'web' }),
    downloadCloudFile: vi.fn().mockRejectedValue({ code: 'web_auth_required', message: '需要网页验证' }),
    completeCloudAuthorization: vi.fn().mockResolvedValue({ id: 'second', displayName: 'second-alice', provider: 'quark', authMethod: 'web' })
  });
  render(<CloudManager provider="quark" client={client} />);
  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.click(screen.getByText('报告.pdf'));
  await user.click(screen.getByRole('button', { name: '下载' }));
  await user.click(screen.getByRole('button', { name: '重新网页授权' }));
  await act(async () => { client.emitAuthorization({ id: 'second', displayName: 'alice', provider: 'quark', authMethod: 'web' }); });
  expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
  expect(screen.queryByRole('tablist')).not.toBeInTheDocument();
  expect(await screen.findByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
});

it('automatically completes web authorization and loads the authorized account', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await act(async () => { client.emitAuthorization({ id: 'second', displayName: 'alice', provider: 'quark', authMethod: 'web' }); });
  expect(await screen.findByRole('region', { name: '夸克网盘文件管理器' }, { timeout: 1500 })).toBeVisible();
  expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
  expect(screen.getByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
});

it('keeps web authorization polling serial while a completion request is still pending', async () => {
  vi.useFakeTimers();
  try {
    let resolveCompletion: ((account: { id: string; displayName: string; provider: 'quark'; authMethod: 'web' }) => void) | undefined;
    const completion = new Promise<{ id: string; displayName: string; provider: 'quark'; authMethod: 'web' }>((resolve) => { resolveCompletion = resolve; });
    const client = createCloudFake({ completeCloudAuthorization: vi.fn().mockReturnValue(completion) });
    render(<CloudManager provider="quark" client={client} />);

    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
    await act(async () => { await Promise.resolve(); });
    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(1);

    await act(async () => { resolveCompletion?.({ id: 'account-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }); await Promise.resolve(); });
  } finally {
    vi.useRealTimers();
  }
});

it('keeps polling after a transient pre-login response and completes when login becomes available', async () => {
  vi.useFakeTimers();
  try {
    const client = createCloudFake({ completeCloudAuthorization: vi.fn()
      .mockRejectedValueOnce({ code: 'provider_error', message: '服务端暂未返回登录账号' })
      .mockResolvedValue({ id: 'account-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }) });
    render(<CloudManager provider="quark" client={client} />);

    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
    await act(async () => { await Promise.resolve(); });
    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(1);

    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(screen.getByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(2);
  } finally {
    vi.useRealTimers();
  }
});

it('reports a successful authorization to an authorization-center host without rendering a file list', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  const onAccountAuthorized = vi.fn();
  render(<CloudManager provider="quark" client={client} authorizationOnly onAccountAuthorized={onAccountAuthorized} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));

  expect(onAccountAuthorized).toHaveBeenCalledWith({ id: 'account-1', displayName: 'alice', provider: 'quark', authMethod: 'web' });
  expect(screen.queryByRole('tree', { name: '夸克网盘文件' })).not.toBeInTheDocument();
});

it('renders an authorized account-management card without loading its directory', async () => {
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} authorizationOnly initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await act(async () => { await Promise.resolve(); });

  expect(screen.getByRole('heading', { name: '夸克网盘 · alice' })).toBeVisible();
  expect(document.querySelector('.cloud-account-summary')).not.toBeInTheDocument();
  expect(screen.queryByText(/官方网页授权.*凭据仅保存在本机/)).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: '已经完成授权' })).toBeDisabled();
  expect(screen.getByRole('button', { name: '退出授权' })).toBeVisible();
  expect(screen.queryByRole('tree')).not.toBeInTheDocument();
  expect(client.listCloudDirectory).not.toHaveBeenCalled();
});

it('can hide session actions in a file-only manager', async () => {
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} showSessionActions={false} />);

  expect(await screen.findByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '已经完成授权' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '退出授权' })).not.toBeInTheDocument();
});

it('logs out only after confirmation and returns to the authorization boundary', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));
  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.click(screen.getByRole('button', { name: '退出授权' }));
  expect(screen.getByRole('alertdialog', { name: '确认退出授权' })).toBeVisible();
  expect(client.logoutCloud).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: '确认退出授权' }));
  expect(client.logoutCloud).toHaveBeenCalledWith('quark');
  expect(await screen.findByRole('region', { name: '夸克网盘账号授权' })).toBeVisible();
  expect(screen.queryByRole('tree', { name: '夸克网盘文件' })).not.toBeInTheDocument();
});

it('accepts the authorized single account and drives the injected directory operations', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));

  expect(await screen.findByRole('region', { name: '夸克网盘文件管理器' })).toBeVisible();
  expect(screen.getByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
  expect(screen.getByText('报告.pdf')).toBeVisible();

  await user.dblClick(screen.getByText('归档'));
  expect(await screen.findByText('说明.txt')).toBeVisible();
  expect(screen.getByRole('navigation', { name: /^当前目录：/ })).toHaveTextContent('根目录');
  expect(client.listCloudDirectory).toHaveBeenLastCalledWith('quark', 'folder-1');

  await user.click(screen.getByRole('button', { name: '新建目录' }));
  await user.type(screen.getByLabelText('目录名称'), '新资料');
  await user.click(screen.getByRole('button', { name: '确认新建目录' }));
  expect(client.createCloudDirectory).toHaveBeenCalledWith('quark', 'folder-1', '新资料');
});

it('uses real checkboxes for row selection and supports select all', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  const { container } = render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('报告.pdf');
  const folderCheckbox = screen.getByRole('checkbox', { name: '选择 归档' });
  const fileCheckbox = screen.getByRole('checkbox', { name: '选择 报告.pdf' });
  expect(folderCheckbox).not.toBeChecked();
  await user.click(screen.getByText('归档'));
  expect(folderCheckbox).toBeChecked();

  await user.click(screen.getByRole('checkbox', { name: '全选当前目录' }));
  expect(folderCheckbox).toBeChecked();
  expect(fileCheckbox).toBeChecked();
  const selectionHeader = screen.getByText('已选择 2 项');
  expect(selectionHeader.closest('.cloud-columns')).not.toBeNull();
  expect(screen.queryByText('名称')).not.toBeInTheDocument();
  expect(container.querySelector('.cloud-selection-summary')).not.toBeInTheDocument();
});

it('keeps file controls on one line and preserves full breadcrumb names in tooltips', async () => {
  const user = userEvent.setup();
  render(<CloudManager provider="quark" client={createCloudFake()} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('归档');
  expect(screen.getByLabelText('文件操作')).toHaveClass('cloud-toolbar--single-line');

  await user.dblClick(screen.getByText('归档'));
  const navigation = screen.getByRole('navigation', { name: /^当前目录：/ });
  expect(within(navigation).getByRole('button', { name: '归档' })).toHaveAttribute('title', '归档');
});

it('keeps only download and delete visible in the selected-file header', async () => {
  const user = userEvent.setup();
  render(<CloudManager provider="quark" client={createCloudFake()} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('报告.pdf');
  await user.click(screen.getByRole('checkbox', { name: '选择 报告.pdf' }));

  const toolbar = screen.getByRole('toolbar', { name: '已选择 1 项的文件操作' });
  expect(within(toolbar).getByRole('button', { name: '下载' })).toBeVisible();
  expect(within(toolbar).getByRole('button', { name: '删除' })).toBeVisible();
  expect(within(toolbar).getByRole('button', { name: '更多文件操作' })).toBeVisible();
  expect(within(toolbar).queryByRole('button', { name: '重命名' })).not.toBeInTheDocument();
  expect(within(toolbar).queryByRole('button', { name: '移动' })).not.toBeInTheDocument();
  expect(within(toolbar).queryByRole('button', { name: '复制到' })).not.toBeInTheDocument();
  expect(within(toolbar).queryByRole('button', { name: '文件详情' })).not.toBeInTheDocument();

  await user.click(within(toolbar).getByRole('button', { name: '更多文件操作' }));
  const menu = screen.getByRole('menu', { name: '更多文件操作' });
  expect(within(menu).getByRole('menuitem', { name: '重命名' })).toBeVisible();
  expect(within(menu).getByRole('menuitem', { name: '移动' })).toBeVisible();
  expect(within(menu).getByRole('menuitem', { name: '复制到' })).toBeVisible();
  expect(within(menu).getByRole('menuitem', { name: '分享' })).toBeVisible();
});

it('formats large cloud files in practical size units', async () => {
  const client = createCloudFake({ listCloudDirectory: vi.fn().mockResolvedValue({ id: '0', items: [{ id: 'large-file', parentId: '0', name: '镜像文件.iso', kind: 'file', size: 3 * 1024 * 1024 * 1024 }] }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('镜像文件.iso');
  expect(screen.getByText('3.0 GB')).toBeVisible();
});

it('sorts cloud files by name, size, and modified time from their column headers', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudDirectory: vi.fn().mockResolvedValue({ id: '0', items: [
    { id: 'largest', parentId: '0', name: 'zeta.txt', kind: 'file', size: 3_000, modifiedAt: '2025-06-03 08:00:00' },
    { id: 'missing', parentId: '0', name: 'middle.txt', kind: 'file' },
    { id: 'smallest', parentId: '0', name: 'alpha.txt', kind: 'file', size: 10, modifiedAt: '2025-06-01 08:00:00' },
    { id: 'medium', parentId: '0', name: 'beta.txt', kind: 'file', size: 200, modifiedAt: '2025-06-02 08:00:00' }
  ] }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  const tree = await screen.findByRole('tree', { name: '夸克网盘文件' });
  const listedNames = () => Array.from(tree.querySelectorAll<HTMLElement>('[role="treeitem"] b[title]'))
    .map((name) => name.getAttribute('title'))
    .filter((name): name is string => name !== null && name !== '回收站' && name !== '我的分享');

  await user.click(screen.getByRole('button', { name: '按名称排序' }));
  expect(listedNames()).toEqual(['alpha.txt', 'beta.txt', 'middle.txt', 'zeta.txt']);
  await user.click(screen.getByRole('button', { name: '按名称排序' }));
  expect(listedNames()).toEqual(['zeta.txt', 'middle.txt', 'beta.txt', 'alpha.txt']);

  await user.click(screen.getByRole('button', { name: '按大小排序' }));
  expect(listedNames()).toEqual(['alpha.txt', 'beta.txt', 'zeta.txt', 'middle.txt']);
  await user.click(screen.getByRole('button', { name: '按大小排序' }));
  expect(listedNames()).toEqual(['zeta.txt', 'beta.txt', 'alpha.txt', 'middle.txt']);

  await user.click(screen.getByRole('button', { name: '按修改时间排序' }));
  expect(listedNames()).toEqual(['alpha.txt', 'beta.txt', 'zeta.txt', 'middle.txt']);
  await user.click(screen.getByRole('button', { name: '按修改时间排序' }));
  expect(listedNames()).toEqual(['zeta.txt', 'beta.txt', 'alpha.txt', 'middle.txt']);
});

it('keeps unavailable cloud file sizes after known sizes in either sort direction', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudDirectory: vi.fn().mockResolvedValue({ id: '0', items: [
    { id: 'null-size', parentId: '0', name: 'null-size.txt', kind: 'file', size: null },
    { id: 'negative-size', parentId: '0', name: 'negative-size.txt', kind: 'file', size: -1 },
    { id: 'large', parentId: '0', name: 'large.txt', kind: 'file', size: 100 },
    { id: 'missing-size', parentId: '0', name: 'missing-size.txt', kind: 'file' },
    { id: 'small', parentId: '0', name: 'small.txt', kind: 'file', size: 1 }
  ] }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  const tree = await screen.findByRole('tree', { name: '夸克网盘文件' });
  const listedNames = () => Array.from(tree.querySelectorAll<HTMLElement>('[role="treeitem"] b[title]'))
    .map((name) => name.getAttribute('title'))
    .filter((name): name is string => name !== null && name !== '回收站' && name !== '我的分享');

  await user.click(screen.getByRole('button', { name: '按大小排序' }));
  expect(listedNames()).toEqual(['small.txt', 'large.txt', 'null-size.txt', 'negative-size.txt', 'missing-size.txt']);
  await user.click(screen.getByRole('button', { name: '按大小排序' }));
  expect(listedNames()).toEqual(['large.txt', 'small.txt', 'null-size.txt', 'negative-size.txt', 'missing-size.txt']);
});

it('keeps Shift range selection anchored to the same file after sorting', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudDirectory: vi.fn().mockResolvedValue({ id: '0', items: [
    { id: 'zeta', parentId: '0', name: 'zeta.txt', kind: 'file', size: 300 },
    { id: 'middle', parentId: '0', name: 'middle.txt', kind: 'file' },
    { id: 'alpha', parentId: '0', name: 'alpha.txt', kind: 'file', size: 10 },
    { id: 'beta', parentId: '0', name: 'beta.txt', kind: 'file', size: 200 }
  ] }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await user.click(await screen.findByText('zeta.txt'));
  await user.click(screen.getByRole('button', { name: '按大小排序' }));
  fireEvent.click(screen.getByText('middle.txt'), { shiftKey: true });

  expect(screen.getByRole('checkbox', { name: '选择 zeta.txt' })).toBeChecked();
  expect(screen.getByRole('checkbox', { name: '选择 middle.txt' })).toBeChecked();
  expect(screen.getByRole('checkbox', { name: '选择 alpha.txt' })).not.toBeChecked();
  expect(screen.getByRole('checkbox', { name: '选择 beta.txt' })).not.toBeChecked();
});

it('activates sort headers by keyboard without opening the selected folder', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await user.click(await screen.findByText('归档'));
  const sizeHeader = screen.getByRole('button', { name: '按大小排序' });
  sizeHeader.focus();
  await user.keyboard('{Enter}');
  expect(sizeHeader).toHaveAttribute('aria-pressed', 'true');
  expect(sizeHeader).toHaveAttribute('aria-description', '当前为升序');
  expect(client.listCloudDirectory).not.toHaveBeenCalledWith('quark', 'folder-1');

  const timeHeader = screen.getByRole('button', { name: '按修改时间排序' });
  timeHeader.focus();
  await user.keyboard(' ');
  expect(timeHeader).toHaveAttribute('aria-pressed', 'true');
  expect(client.listCloudDirectory).not.toHaveBeenCalledWith('quark', 'folder-1');
});

it('keeps file-list keyboard shortcuts available from selection checkboxes', async () => {
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('报告.pdf');
  const fileCheckbox = screen.getByRole('checkbox', { name: '选择 报告.pdf' });
  fileCheckbox.focus();
  fireEvent.keyDown(fileCheckbox, { key: 'a', ctrlKey: true });
  expect(screen.getByRole('checkbox', { name: '选择 归档' })).toBeChecked();
  expect(fileCheckbox).toBeChecked();

  fireEvent.keyDown(fileCheckbox, { key: 'Delete' });
  expect(screen.getByRole('alertdialog', { name: '确认移入回收站' })).toBeVisible();
});

it('opens folders by double click without rendering an enter button', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('归档');
  expect(screen.queryByRole('button', { name: '进入 归档' })).not.toBeInTheDocument();
  await user.dblClick(screen.getByText('归档'));
  expect(await screen.findByText('说明.txt')).toBeVisible();
  expect(client.listCloudDirectory).toHaveBeenLastCalledWith('quark', 'folder-1');
});

it.each(['quark', 'pikpak', 'baidu', 'tianyi', 'aliyun', 'xunlei', 'uc'] as const)(
  'uses the named parent-directory row instead of a back button for %s', async (provider) => {
    const user = userEvent.setup();
    const listCloudDirectory = vi.fn().mockImplementation((_provider: CloudProvider, parentId: string) => Promise.resolve(parentId === 'child-folder'
      ? { id: parentId, items: [{ id: 'child-file', parentId, name: '子目录文件.txt', kind: 'file' as const }] }
      : { id: parentId, items: [{ id: 'child-folder', parentId, name: '子目录', kind: 'folder' as const }] }));
    const client = createCloudFake({ listCloudDirectory });
    render(<CloudManager provider={provider} client={client} initialAccount={{ id: `${provider}-account`, displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

    await screen.findByText('子目录');
    expect(screen.queryByRole('button', { name: '返回上一级' })).not.toBeInTheDocument();
    await user.dblClick(screen.getByText('子目录'));
    const parentRow = await screen.findByRole('treeitem', { name: '返回上一级目录' });
    expect(parentRow).toHaveTextContent('返回上一级目录');
    expect(within(parentRow).getByRole('checkbox', { name: '选择 返回上一级目录' })).toBeDisabled();
    await user.click(parentRow);
    await waitFor(() => expect(listCloudDirectory).toHaveBeenLastCalledWith(provider, cloudRootDirectoryId[provider]));
  }
);

it.each(['回收站', '我的分享'] as const)(
  'returns from the virtual %s folder to the cloud root with the named parent-directory row', async (virtualFolder) => {
    const user = userEvent.setup();
    const provider = 'pikpak' as const;
    const recycleBinId = cloudRecycleBinDirectoryId[provider];
    const listCloudDirectory = vi.fn().mockImplementation((_requestedProvider: CloudProvider, parentId: string) => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [{ id: 'deleted-file', parentId, name: '已删除文件.txt', kind: 'file' as const }] }
      : { id: parentId, items: [] }));
    const listCloudShares = vi.fn().mockResolvedValue([{ id: 'share-1', url: 'https://mypikpak.com/s/share-1', name: '已分享文件.txt', kind: 'file' as const, sourceFileIds: [] }]);
    const client = createCloudFake({ listCloudDirectory, listCloudShares });
    render(<CloudManager provider={provider} client={client} initialAccount={{ id: 'pikpak-account', displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

    await user.dblClick(await screen.findByText(virtualFolder, { exact: true }));
    const parentRow = await screen.findByRole('treeitem', { name: '返回上一级目录' });
    expect(parentRow).toHaveTextContent('返回上一级目录');
    expect(within(parentRow).getByRole('checkbox', { name: '选择 返回上一级目录' })).toBeDisabled();
    await user.click(parentRow);

    await waitFor(() => expect(listCloudDirectory).toHaveBeenLastCalledWith(provider, cloudRootDirectoryId[provider]));
  }
);

it('shows normal file commands in the row context menu and opens a folder from it', async () => {
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('归档');
  fireEvent.contextMenu(screen.getByText('归档'), { clientX: 300, clientY: 240 });
  const menu = screen.getByRole('menu', { name: '归档 文件菜单' });
  expect(screen.getByRole('menuitem', { name: '打开' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '下载' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '重命名' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '移动' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '复制到' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '分享' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '删除' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '文件详情' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '移入超级保险箱' })).not.toBeInTheDocument();
  expect(menu).toBeVisible();

  fireEvent.click(screen.getByRole('menuitem', { name: '打开' }));
  expect(await screen.findByText('说明.txt')).toBeVisible();
});

it('uses readable file-manager text with compact file rows', async () => {
  const style = document.createElement('style');
  style.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(style);
  try {
    const { container } = render(<CloudManager provider="quark" client={createCloudFake()} initialAccount={{ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);
    const folder = await screen.findByText('归档');
    const row = folder.closest('.cloud-file-row')!;
    expect(style.textContent).toMatch(/\.cloud-list\{[^}]*scrollbar-gutter:stable/);
    expect(style.textContent).toMatch(/\.cloud-file-manager \.cloud-row::after\{[^}]*right:calc\(-1 \* var\(--cloud-scrollbar-gutter-width/);
    expect(container.querySelector('.cloud-list')!.style.getPropertyValue('--cloud-scrollbar-gutter-width')).toBe('0px');
    expect(getComputedStyle(row).fontSize).toBe('14px');
    expect(getComputedStyle(row).minHeight).toBe('44px');
    expect(getComputedStyle(document.querySelector('.cloud-columns')!).fontSize).toBe('13px');
    expect(getComputedStyle(screen.getByRole('button', { name: '上传', exact: true })).fontSize).toBe('14px');
  } finally { style.remove(); }
});

it('uses smaller cloud-card typography while preserving the signed-in account name size', async () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const client = createCloudFake();
    render(<div className="cloud-provider-auths"><CloudManager provider="quark" client={client} authorizationOnly initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} /></div>);

    await act(async () => { await Promise.resolve(); });
    const heading = screen.getByRole('heading', { name: '夸克网盘 · alice' });
    const accountName = heading.querySelector<HTMLElement>('.cloud-account-name')!;
    const authorizationState = screen.getByRole('button', { name: '已经完成授权' });
    const logout = screen.getByRole('button', { name: '退出授权' });

    expect(getComputedStyle(heading).fontSize).toBe('18px');
    expect(getComputedStyle(authorizationState).fontSize).toBe('13px');
    expect(getComputedStyle(logout).fontSize).toBe('13px');
    expect(getComputedStyle(accountName).fontSize).toBe('13px');
  } finally {
    stylesheet.remove();
  }
});

it('keeps an authorization card at the connected-card height while its provider information is open', async () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const user = userEvent.setup();
    render(<div className="cloud-provider-auths"><CloudManager provider="uc" client={createCloudFake()} authorizationOnly initialAccount={{ id: 'uc-1', displayName: 'alice', provider: 'uc', authMethod: 'web' }} restoreOnMount={false} /></div>);

    await act(async () => { await Promise.resolve(); });
    const card = screen.getByRole('region', { name: 'UC 网盘账号授权' });
    expect(getComputedStyle(card).minHeight).toBe('168px');

    await user.click(screen.getByRole('button', { name: '查看UC 网盘信息' }));
    expect(screen.getByRole('region', { name: 'UC 网盘信息' })).toBeVisible();
    expect(getComputedStyle(card).minHeight).toBe('168px');
  } finally {
    stylesheet.remove();
  }
});

it('repositions a bottom file context menu using its rendered height', async () => {
  const innerWidth = vi.spyOn(window, 'innerWidth', 'get').mockReturnValue(900);
  const innerHeight = vi.spyOn(window, 'innerHeight', 'get').mockReturnValue(500);
  const bounds = vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function () {
    if (this.classList.contains('cloud-context-menu')) {
      return { width: 210, height: 350, top: 0, right: 210, bottom: 350, left: 0, x: 0, y: 0, toJSON: () => ({}) };
    }
    return { width: 0, height: 0, top: 0, right: 0, bottom: 0, left: 0, x: 0, y: 0, toJSON: () => ({}) };
  });

  try {
    render(<CloudManager provider="quark" client={createCloudFake()} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

    await screen.findByText('报告.pdf');
    fireEvent.contextMenu(screen.getByText('报告.pdf'), { clientX: 300, clientY: 480 });

    await waitFor(() => expect(screen.getByRole('menu', { name: '报告.pdf 文件菜单' })).toHaveStyle({ top: '142px' }));
  } finally {
    bounds.mockRestore();
    innerHeight.mockRestore();
    innerWidth.mockRestore();
  }
});

it('copies selected files through a browsable destination picker', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('报告.pdf');
  fireEvent.contextMenu(screen.getByText('报告.pdf'), { clientX: 300, clientY: 240 });
  await user.click(screen.getByRole('menuitem', { name: '复制到' }));
  const picker = screen.getByRole('dialog', { name: '选择复制位置' });
  expect(picker).toBeVisible();
  await user.click(screen.getByRole('button', { name: '打开 归档' }));
  expect(screen.getByRole('navigation', { name: '目标目录' })).toHaveTextContent('根目录›归档');
  await user.click(screen.getByRole('button', { name: '返回根目录' }));
  await user.click(screen.getByRole('button', { name: '复制到此处' }));

  expect(client.copyCloudFiles).toHaveBeenCalledWith('quark', ['file-1'], '0');
  expect(await screen.findByRole('status')).toHaveTextContent('复制成功');
  await user.dblClick(screen.getByText('归档'));
  await waitFor(() => expect(screen.queryByText('复制成功')).not.toBeInTheDocument());
});

it('retries the destination directory that actually failed to load', async () => {
  const user = userEvent.setup();
  let folderAttempts = 0;
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockImplementation((_provider: string, parentId = '0') => {
      if (parentId === 'folder-1') {
        folderAttempts += 1;
        if (folderAttempts === 1) return Promise.reject(new Error('目录暂时不可用'));
        return Promise.resolve({ id: 'folder-1', items: [] });
      }
      return Promise.resolve({ id: '0', items: [{ id: 'folder-1', parentId: '0', name: '归档', kind: 'folder' }, { id: 'file-1', parentId: '0', name: '报告.pdf', kind: 'file' }] });
    })
  });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('报告.pdf');
  fireEvent.contextMenu(screen.getByText('报告.pdf'), { clientX: 300, clientY: 240 });
  await user.click(screen.getByRole('menuitem', { name: '复制到' }));
  await user.click(screen.getByRole('button', { name: '打开 归档' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('目录暂时不可用');
  expect(screen.getByRole('navigation', { name: '目标目录' })).toHaveTextContent('根目录›归档');

  await user.click(screen.getByRole('button', { name: '重试' }));
  expect(await screen.findByText('此目录没有子文件夹')).toBeVisible();
  expect(client.listCloudDirectory).toHaveBeenLastCalledWith('quark', 'folder-1');
  await user.click(screen.getByRole('button', { name: '复制到此处' }));
  expect(client.copyCloudFiles).toHaveBeenCalledWith('quark', ['file-1'], 'folder-1');
});

it('closes the destination picker and requires official web authorization when its directory read needs verification', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({
    listCloudDirectory: vi.fn()
      .mockResolvedValueOnce({ id: '0', items: [{ id: 'folder-1', parentId: '0', name: '归档', kind: 'folder' }, { id: 'file-1', parentId: '0', name: '报告.pdf', kind: 'file' }] })
      .mockRejectedValueOnce({ code: 'web_auth_required', message: '需要网页验证' })
      .mockResolvedValue({ id: '0', items: [{ id: 'folder-1', parentId: '0', name: '归档', kind: 'folder' }] })
  });
  render(<CloudManager provider="xunlei" client={client} initialAccount={{ id: 'x-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('报告.pdf');
  fireEvent.contextMenu(screen.getByText('报告.pdf'), { clientX: 300, clientY: 240 });
  await user.click(screen.getByRole('menuitem', { name: '移动' }));

  expect(await screen.findByRole('button', { name: '打开 迅雷网盘 授权并继续选择移动目标' })).toBeVisible();
  expect(screen.queryByRole('dialog', { name: '选择移动位置' })).not.toBeInTheDocument();
  expect(screen.getByRole('region', { name: '迅雷网盘文件管理器' })).toBeVisible();
  expect(client.moveCloudFiles).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: '打开 迅雷网盘 授权并继续选择移动目标' }));
  expect(client.beginCloudAuthorization).toHaveBeenCalledWith('xunlei');
  await act(async () => { client.emitAuthorization({ id: 'x-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }); });
  expect(await screen.findByRole('dialog', { name: '选择移动位置' })).toBeVisible();
});

it('normalizes provider modified times in the list while preserving valid Tianyi timestamps', async () => {
  const user = userEvent.setup();
  const timestamp = 1_735_787_045_000;
  const date = new Date(timestamp);
  const localTimestamp = `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')} ${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}:${String(date.getSeconds()).padStart(2, '0')}`;
  const offsetDate = new Date('2025-01-02T03:04:05+08:00');
  const localOffsetTimestamp = `${offsetDate.getFullYear()}-${String(offsetDate.getMonth() + 1).padStart(2, '0')}-${String(offsetDate.getDate()).padStart(2, '0')} ${String(offsetDate.getHours()).padStart(2, '0')}:${String(offsetDate.getMinutes()).padStart(2, '0')}:${String(offsetDate.getSeconds()).padStart(2, '0')}`;
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockResolvedValue({ id: '0', items: [
      { id: 'seconds', parentId: '0', name: 'seconds.txt', kind: 'file', modifiedAt: String(timestamp / 1_000) },
      { id: 'milliseconds', parentId: '0', name: 'milliseconds.txt', kind: 'file', modifiedAt: String(timestamp) },
      { id: 'iso', parentId: '0', name: 'iso.txt', kind: 'file', modifiedAt: '2025-01-02T03:04:05.000Z' },
      { id: 'iso-offset', parentId: '0', name: 'iso-offset.txt', kind: 'file', modifiedAt: '2025-01-02T03:04:05+08:00' },
      { id: 'date-only', parentId: '0', name: 'date-only.txt', kind: 'file', modifiedAt: '2025-06-07' },
      { id: 'minute', parentId: '0', name: 'minute.txt', kind: 'file', modifiedAt: '2025-06-07 08:09' },
      { id: 'tianyi', parentId: '0', name: 'tianyi.txt', kind: 'file', modifiedAt: '2025-06-07 08:09:10' },
      { id: 'dst-canonical', parentId: '0', name: 'dst-canonical.txt', kind: 'file', modifiedAt: '2025-03-09 02:30:00' },
      { id: 'invalid-canonical', parentId: '0', name: 'invalid-canonical.txt', kind: 'file', size: 1024, modifiedAt: '2025-02-30 08:09:10' },
      { id: 'invalid-time', parentId: '0', name: 'invalid-time.txt', kind: 'file', size: 1024, modifiedAt: '2025-99-99 99:99:99' },
      { id: 'unknown', parentId: '0', name: 'unknown.txt', kind: 'file', size: 1024, modifiedAt: '昨天' },
      { id: 'missing', parentId: '0', name: 'missing.txt', kind: 'file', size: 1024 }
    ] })
  });
  render(<CloudManager provider="tianyi" client={client} initialAccount={{ id: 't-1', displayName: 'alice', provider: 'tianyi', authMethod: 'web' }} restoreOnMount={false} />);

  const tree = await screen.findByRole('tree', { name: '天翼云盘文件' });
  expect(within(tree).getByText('seconds.txt').closest('[role="treeitem"]')).toHaveTextContent(localTimestamp);
  expect(within(tree).getByText('milliseconds.txt').closest('[role="treeitem"]')).toHaveTextContent(localTimestamp);
  expect(within(tree).getByText('iso.txt').closest('[role="treeitem"]')).toHaveTextContent(localTimestamp);
  expect(within(tree).getByText('iso-offset.txt').closest('[role="treeitem"]')).toHaveTextContent(localOffsetTimestamp);
  expect(within(tree).getByText('date-only.txt').closest('[role="treeitem"]')).toHaveTextContent('2025-06-07 00:00:00');
  expect(within(tree).getByText('minute.txt').closest('[role="treeitem"]')).toHaveTextContent('2025-06-07 08:09:00');
  expect(within(tree).getByText('tianyi.txt').closest('[role="treeitem"]')).toHaveTextContent('2025-06-07 08:09:10');
  expect(within(tree).getByText('dst-canonical.txt').closest('[role="treeitem"]')).toHaveTextContent('2025-03-09 02:30:00');
  expect(within(tree).getByText('invalid-canonical.txt').closest('[role="treeitem"]')).toHaveTextContent('—');
  expect(within(tree).getByText('invalid-time.txt').closest('[role="treeitem"]')).toHaveTextContent('—');
  expect(within(tree).getByText('unknown.txt').closest('[role="treeitem"]')).toHaveTextContent('—');
  expect(within(tree).getByText('missing.txt').closest('[role="treeitem"]')).toHaveTextContent('—');

});

it('keeps multi-selection context actions valid for every selected item', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('归档');
  await user.click(screen.getByRole('checkbox', { name: '全选当前目录' }));
  fireEvent.contextMenu(screen.getByText('归档'), { clientX: 300, clientY: 240 });
  expect(screen.getByRole('menu', { name: '归档 文件菜单' })).toHaveTextContent('已选择 2 项');
  expect(screen.queryByRole('menuitem', { name: '打开' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '重命名' })).not.toBeInTheDocument();
  expect(screen.getByRole('menuitem', { name: '移动' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '删除' })).toBeVisible();
});

it('shows create, upload and refresh commands when right-clicking file-list whitespace', async () => {
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  const tree = await screen.findByRole('tree', { name: '夸克网盘文件' });
  fireEvent.click(screen.getByText('报告.pdf'));
  expect(screen.getByRole('checkbox', { name: '选择 报告.pdf' })).toBeChecked();
  fireEvent.contextMenu(tree, { clientX: 420, clientY: 320 });
  expect(screen.getByRole('checkbox', { name: '选择 报告.pdf' })).not.toBeChecked();
  expect(screen.getByRole('menu', { name: '文件区菜单' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '新建目录' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '上传' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '刷新' })).toBeVisible();
});

it('supports common file-manager keyboard shortcuts', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  const tree = await screen.findByRole('tree', { name: '夸克网盘文件' });
  tree.focus();
  fireEvent.keyDown(tree, { key: 'a', ctrlKey: true });
  expect(screen.getByRole('checkbox', { name: '选择 归档' })).toBeChecked();
  expect(screen.getByRole('checkbox', { name: '选择 报告.pdf' })).toBeChecked();

  await user.click(screen.getByText('报告.pdf'));
  fireEvent.keyDown(tree, { key: 'F2' });
  expect(screen.getByLabelText('新名称')).toHaveValue('报告.pdf');
  fireEvent.keyDown(tree, { key: 'Escape' });
  expect(screen.queryByLabelText('新名称')).not.toBeInTheDocument();
  fireEvent.keyDown(tree, { key: 'Delete' });
  expect(screen.getByRole('alertdialog', { name: '确认移入回收站' })).toBeVisible();
});

it('renders an inline authorization continuation when the initial directory credential expires', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudDirectory: vi.fn().mockRejectedValue(Object.assign(new Error('会话失效'), { code: 'credentials_expired' })) });
  render(<CloudManager provider="pikpak" client={client} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('会话失效');
  expect(screen.getByRole('button', { name: '打开 PikPak 网盘 授权并继续加载目录' })).toBeVisible();
  expect(screen.getByRole('region', { name: 'PikPak 网盘文件管理器' })).toBeVisible();
  expect(screen.queryByRole('tree', { name: 'PikPak 网盘文件' })).not.toBeInTheDocument();
});

it('requires a recycle-bin confirmation before deleting selected files', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));
  await screen.findByText('报告.pdf');
  await user.click(screen.getByText('报告.pdf'));
  await user.click(screen.getByRole('button', { name: '删除' }));

  expect(screen.getByRole('alertdialog', { name: '确认移入回收站' })).toHaveTextContent('不会永久删除');
  expect(client.trashCloudFiles).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: '移入回收站' }));
  expect(client.trashCloudFiles).toHaveBeenCalledWith('quark', ['file-1']);
  expect(await screen.findByRole('status')).toHaveTextContent('删除成功');
});

it.each(['quark', 'pikpak', 'baidu', 'tianyi', 'aliyun', 'xunlei', 'uc'] as const)('renders the %s virtual recycle bin with a trash-can icon', async (provider) => {
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockResolvedValue({ id: cloudRootDirectoryId[provider], items: [
      { id: 'ordinary-folder', parentId: cloudRootDirectoryId[provider], name: '普通文件夹', kind: 'folder' },
      { id: 'ordinary-file', parentId: cloudRootDirectoryId[provider], name: '普通文件.txt', kind: 'file' }
    ] })
  });
  render(<CloudManager provider={provider} client={client} initialAccount={{ id: `${provider}-1`, displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

  const recycleBinName = await screen.findByTitle('回收站');
  expect(recycleBinName.previousElementSibling).toHaveClass('recycle-bin');
  expect(screen.getByTitle('普通文件夹').previousElementSibling).toHaveClass('folder');
  expect(screen.getByTitle('普通文件.txt').previousElementSibling).toHaveClass('file');
});

it.each(['quark', 'uc', 'baidu', 'tianyi', 'aliyun', 'xunlei', 'pikpak'] as const)('shows a virtual recycle bin and recycle-only controls for %s', async (provider) => {
  const user = userEvent.setup();
  const recycleBinId = cloudRecycleBinDirectoryId[provider];
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockImplementation((_requestedProvider: CloudProvider, parentId = '0') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [{ id: 'deleted-file', parentId: recycleBinId, name: '已删除文件.txt', kind: 'file', size: 12 }] }
      : { id: parentId, items: [{ id: 'live-file', parentId, name: '仍在网盘的文件.txt', kind: 'file', size: 10 }] }))
  });
  render(<CloudManager provider={provider} client={client} initialAccount={{ id: `${provider}-1`, displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

  await user.dblClick(await screen.findByText('回收站'));
  await screen.findByText('已删除文件.txt');
  expect(screen.queryByRole('button', { name: '上传' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '新建目录' })).not.toBeInTheDocument();

  await user.click(screen.getByText('已删除文件.txt'));
  const toolbar = screen.getByRole('toolbar', { name: '已选择 1 项的文件操作' });
  expect(within(toolbar).getByRole('button', { name: '恢复' })).toBeVisible();
  expect(within(toolbar).getByRole('button', { name: '永久删除' })).toBeVisible();
  expect(within(toolbar).queryByRole('button', { name: '下载' })).not.toBeInTheDocument();
});

it('restores and permanently deletes non-PikPak recycle records after confirmation', async () => {
  const user = userEvent.setup();
  const provider = 'quark' as const;
  const recycleBinId = cloudRecycleBinDirectoryId[provider];
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockImplementation((_requestedProvider: CloudProvider, parentId = '0') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [{ id: 'quark-recycle-record', parentId: recycleBinId, name: '已删除文件.txt', kind: 'file', size: 12 }] }
      : { id: parentId, items: [] }))
  });
  render(<CloudManager provider={provider} client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

  await user.dblClick(await screen.findByText('回收站'));
  await user.click(await screen.findByText('已删除文件.txt'));
  await user.click(screen.getByRole('button', { name: '恢复' }));
  expect(client.restoreCloudFiles).toHaveBeenCalledWith('quark', ['quark-recycle-record']);

  await screen.findByText('已删除文件.txt');
  await user.click(screen.getByText('已删除文件.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  const dialog = screen.getByRole('alertdialog', { name: '确认永久删除' });
  expect(dialog).toHaveTextContent('不可恢复');
  await user.click(within(dialog).getByRole('button', { name: '永久删除', exact: true }));
  expect(client.deleteCloudFiles).toHaveBeenCalledWith('quark', ['quark-recycle-record']);
  expect(client.trashCloudFiles).not.toHaveBeenCalled();
});

it('limits PikPak recycle-bin items to restore and permanent deletion without folder navigation', async () => {
  const user = userEvent.setup();
  const recycleBinId = '__versa_down_pikpak_recycle_bin__';
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockImplementation((_provider: string, parentId = '0') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [
        { id: 'deleted-file', parentId: recycleBinId, name: '已删除文件.txt', kind: 'file', size: 12 },
        { id: 'deleted-folder', parentId: recycleBinId, name: '已删除文件夹', kind: 'folder' }
      ] }
      : { id: '0', items: [
        { id: recycleBinId, parentId: '0', name: '回收站', kind: 'folder' },
        { id: 'live-file', parentId: '0', name: '仍在网盘的文件.txt', kind: 'file', size: 10 }
      ] }))
  });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('回收站');
  fireEvent.doubleClick(screen.getByText('回收站'));
  const tree = await screen.findByRole('tree', { name: 'PikPak 网盘文件' });
  await screen.findByText('已删除文件.txt');
  expect(screen.queryByRole('button', { name: '上传' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '新建目录' })).not.toBeInTheDocument();

  fireEvent.contextMenu(tree, { clientX: 260, clientY: 220 });
  expect(screen.queryByRole('menuitem', { name: '新建目录' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '上传文件' })).not.toBeInTheDocument();
  expect(screen.getByRole('menuitem', { name: '刷新' })).toBeVisible();

  const deletedFolder = screen.getByTitle('已删除文件夹');
  fireEvent.contextMenu(deletedFolder, { clientX: 300, clientY: 240 });
  expect(screen.queryByRole('menuitem', { name: '打开' })).not.toBeInTheDocument();
  expect(screen.getByRole('menuitem', { name: '恢复' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '永久删除' })).toBeVisible();
  client.listCloudDirectory.mockClear();
  fireEvent.doubleClick(deletedFolder);
  fireEvent.keyDown(tree, { key: 'Enter' });
  expect(client.listCloudDirectory).not.toHaveBeenCalled();

  await user.click(screen.getByText('已删除文件.txt'));
  await user.click(screen.getByRole('button', { name: '恢复' }));
  expect(client.restoreCloudFiles).toHaveBeenCalledWith('pikpak', ['deleted-file']);

  await screen.findByText('已删除文件.txt');
  await user.click(screen.getByText('已删除文件.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  const permanentDeleteDialog = screen.getByRole('alertdialog', { name: '确认永久删除' });
  expect(permanentDeleteDialog).toHaveTextContent('不可恢复');
  expect(client.deleteCloudFiles).not.toHaveBeenCalled();
  await user.click(within(permanentDeleteDialog).getByRole('button', { name: '永久删除', exact: true }));
  expect(client.deleteCloudFiles).toHaveBeenCalledWith('pikpak', ['deleted-file']);
  expect(client.trashCloudFiles).not.toHaveBeenCalled();
});

it('excludes the PikPak recycle-bin virtual folder from move destinations', async () => {
  const user = userEvent.setup();
  const recycleBinId = '__versa_down_pikpak_recycle_bin__';
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockResolvedValue({ id: '0', items: [
      { id: recycleBinId, parentId: '0', name: '回收站', kind: 'folder' },
      { id: 'live-file', parentId: '0', name: '仍在网盘的文件.txt', kind: 'file', size: 10 }
    ] })
  });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('仍在网盘的文件.txt');
  await user.click(screen.getByText('仍在网盘的文件.txt'));
  await user.click(screen.getByRole('button', { name: '更多文件操作' }));
  await user.click(screen.getByRole('menuitem', { name: '移动' }));

  const picker = await screen.findByRole('dialog', { name: '选择移动位置' });
  expect(picker).not.toHaveTextContent('回收站');
  expect(screen.queryByRole('button', { name: '打开 回收站' })).not.toBeInTheDocument();
});

it('refreshes the current directory when a completed move is indexed after the first listing', async () => {
  const user = userEvent.setup();
  const sourceFile = { id: 'move-source', parentId: '0', name: '待移动文件.txt', kind: 'file' as const };
  const destination = { id: 'archive', parentId: '0', name: '归档', kind: 'folder' as const };
  let moveCompleted = false;
  let sourceReadsAfterMove = 0;
  const listCloudDirectory = vi.fn(async (_provider: string, parentId = '0') => {
    if (parentId === destination.id) return { id: destination.id, items: [] };
    const items = moveCompleted && sourceReadsAfterMove++ > 0 ? [destination] : [sourceFile, destination];
    return { id: '0', items };
  });
  const client = createCloudFake({
    listCloudDirectory,
    moveCloudFiles: vi.fn().mockImplementation(async () => { moveCompleted = true; })
  });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText(sourceFile.name);
  await user.click(screen.getByText(sourceFile.name));
  await user.click(screen.getByRole('button', { name: '更多文件操作' }));
  await user.click(screen.getByRole('menuitem', { name: '移动' }));
  await user.click(await screen.findByRole('button', { name: `打开 ${destination.name}` }));
  await user.click(screen.getByRole('button', { name: '移动到此处' }));

  await waitFor(() => expect(screen.queryByText(sourceFile.name)).not.toBeInTheDocument());
  expect(await screen.findByRole('status')).toHaveTextContent('移动成功');
  await user.dblClick(screen.getByText(destination.name));
  await waitFor(() => expect(screen.queryByText('移动成功')).not.toBeInTheDocument());
});

it('treats the root PikPak recycle-bin folder as navigation-only', async () => {
  const user = userEvent.setup();
  const recycleBinId = '__versa_down_pikpak_recycle_bin__';
  const client = createCloudFake({
    listCloudDirectory: vi.fn().mockImplementation((_provider: string, parentId = '0') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [] }
      : { id: '0', items: [{ id: recycleBinId, parentId: '0', name: '回收站', kind: 'folder' }] }))
  });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);

  const recycleBin = await screen.findByText('回收站');
  expect(screen.getByRole('checkbox', { name: '选择 回收站' })).toBeDisabled();
  await user.click(recycleBin);
  expect(screen.queryByRole('toolbar', { name: /已选择/ })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '下载' })).not.toBeInTheDocument();
  fireEvent.keyDown(screen.getByRole('tree', { name: 'PikPak 网盘文件' }), { key: 'Delete' });
  expect(screen.queryByRole('alertdialog', { name: '确认移入回收站' })).not.toBeInTheDocument();

  fireEvent.contextMenu(recycleBin, { clientX: 300, clientY: 240 });
  expect(screen.getByRole('menuitem', { name: '打开' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '下载' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '删除' })).not.toBeInTheDocument();
  fireEvent.doubleClick(recycleBin);
  await waitFor(() => expect(client.listCloudDirectory).toHaveBeenLastCalledWith('pikpak', recycleBinId));
});

it('keeps root virtual folders out of selection while retaining their navigation menu', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  const recycleBin = await screen.findByText('回收站');
  const myShares = screen.getByText('我的分享');
  expect(screen.getByRole('checkbox', { name: '选择 回收站' })).toBeDisabled();
  expect(screen.getByRole('checkbox', { name: '选择 我的分享' })).toBeDisabled();

  await user.click(recycleBin);
  await user.click(myShares);
  expect(screen.queryByRole('toolbar', { name: /已选择/ })).not.toBeInTheDocument();

  fireEvent.contextMenu(myShares, { clientX: 300, clientY: 240 });
  expect(screen.getByRole('menuitem', { name: '打开' })).toBeVisible();
  expect(screen.queryByRole('toolbar', { name: /已选择/ })).not.toBeInTheDocument();
});

it('queues a selected cloud folder as one directory parent task', async () => {
  const user = userEvent.setup();
  const onDownloadTask = vi.fn();
  const queuedTask = { id: 'folder-download-1', name: '归档', kind: 'folder' as const, status: 'queued' as const, progress: 0 };
  const root = { id: 'group-root', name: '归档', kind: 'folder' as const, status: 'queued' as const, progress: 0, children: [queuedTask] };
  const downloadCloudDirectory = vi.fn().mockResolvedValue({ task: { task: queuedTask, queueTask: root }, queuedCount: 1, failures: [] });
  const client = createCloudFake({ downloadCloudDirectory });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} onDownloadTask={onDownloadTask} />);

  await screen.findByText('归档');
  await user.click(screen.getByRole('checkbox', { name: '选择 归档' }));
  await user.click(screen.getByRole('button', { name: '下载' }));

  await waitFor(() => expect(downloadCloudDirectory).toHaveBeenCalledWith('quark', 'folder-1', '归档'));
  expect(client.downloadCloudFile).not.toHaveBeenCalled();
  expect(onDownloadTask).toHaveBeenCalledTimes(1);
  expect(onDownloadTask).toHaveBeenCalledWith({ task: queuedTask, queueTask: root });
});

it('forwards a completed directory duplicate without discarding its duplicate outcome', async () => {
  const user = userEvent.setup();
  const onDownloadTask = vi.fn();
  const existing = { id: 'existing-folder', name: '归档', kind: 'folder' as const, status: 'completed' as const, progress: 100 };
  const root = { ...existing, children: [{ id: 'existing-file', name: '文件.bin', kind: 'cloud' as const, status: 'completed' as const, progress: 100 }] };
  const duplicate = { duplicate: { task: existing, state: 'completed_files_present' as const }, queueTask: root };
  const downloadCloudDirectory = vi.fn().mockResolvedValue({ task: duplicate, queuedCount: 0, failures: [] });
  const client = createCloudFake({ downloadCloudDirectory });
  render(<CloudManager provider="xunlei" client={client} initialAccount={{ id: 'x-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }} restoreOnMount={false} onDownloadTask={onDownloadTask} />);

  await screen.findByText('归档');
  await user.click(screen.getByRole('checkbox', { name: '选择 归档' }));
  await user.click(screen.getByRole('button', { name: '下载' }));

  await waitFor(() => expect(onDownloadTask).toHaveBeenCalledWith(duplicate));
});

it('requires an ordinary-download confirmation before queuing a cloud file when privacy downloads are enabled', async () => {
  const user = userEvent.setup();
  const downloadCloudFile = vi.fn().mockResolvedValue({ task: { id: 'file-download-1', name: '报告.pdf', kind: 'cloud', status: 'queued', progress: 0 } });
  const client = createCloudFake({ downloadCloudFile });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} privacyDownloadEnabled />);

  await screen.findByText('报告.pdf');
  await user.click(screen.getByRole('checkbox', { name: '选择 报告.pdf' }));
  await user.click(screen.getByRole('button', { name: '下载' }));

  const confirmation = screen.getByRole('alertdialog', { name: '确认下载内容' });
  expect(downloadCloudFile).not.toHaveBeenCalled();
  await user.click(within(confirmation).getByRole('button', { name: '立即下载' }));

  await waitFor(() => expect(downloadCloudFile).toHaveBeenCalledWith('quark', 'file-1', '报告.pdf', false));
});

it('marks a selected cloud folder private only after the private-download confirmation', async () => {
  const user = userEvent.setup();
  const queuedTask = { id: 'folder-download-private', name: '归档', kind: 'folder' as const, status: 'queued' as const, progress: 0 };
  const downloadCloudDirectory = vi.fn().mockResolvedValue({ task: { task: queuedTask }, queuedCount: 1, failures: [] });
  const client = createCloudFake({ downloadCloudDirectory });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} privacyDownloadEnabled />);

  await screen.findByText('归档');
  await user.click(screen.getByRole('checkbox', { name: '选择 归档' }));
  await user.click(screen.getByRole('button', { name: '下载' }));

  const confirmation = screen.getByRole('alertdialog', { name: '确认下载内容' });
  expect(downloadCloudDirectory).not.toHaveBeenCalled();
  await user.click(within(confirmation).getByRole('button', { name: '隐私下载' }));

  await waitFor(() => expect(downloadCloudDirectory).toHaveBeenCalledWith('quark', 'folder-1', '归档', true));
});

it('reports queued count and redacted per-file directory download failures instead of silently succeeding', async () => {
  const user = userEvent.setup();
  const queuedTask = { id: 'folder-download-1', name: '归档', kind: 'folder' as const, status: 'queued' as const, progress: 0 };
  const client = createCloudFake({
    downloadCloudDirectory: vi.fn().mockResolvedValue({
      queuedCount: 1,
      task: { task: queuedTask },
      failures: [{ relativePath: '归档/私密文件.txt', code: 'network_error', message: '网络或下载引擎错误' }]
    })
  });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByText('归档');
  await user.click(screen.getByRole('checkbox', { name: '选择 归档' }));
  await user.click(screen.getByRole('button', { name: '下载' }));

  expect(await screen.findByRole('alert')).toHaveTextContent('目录下载已入队 1 个文件；归档/私密文件.txt：网络或下载引擎错误');
});

it('keeps the current file list visible when a cloud operation fails', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ downloadCloudFile: vi.fn().mockRejectedValue(new Error('网络断开')) });
  render(<CloudManager provider="quark" client={client} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));
  await screen.findByText('报告.pdf');
  await user.click(screen.getByText('报告.pdf'));
  await user.click(screen.getByRole('button', { name: '下载' }));

  expect(await screen.findByRole('alert')).toHaveTextContent('下载失败');
  expect(screen.getByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
  expect(screen.getByText('报告.pdf')).toBeVisible();
});

it.each([
  ['pikpak', 'PikPak 网盘'],
  ['baidu', '百度网盘'],
  ['xunlei', '迅雷网盘']
] as const)('offers %s reauthorization after a download requires official web verification', async (provider, label) => {
  const user = userEvent.setup();
  const client = createCloudFake({
    downloadCloudFile: vi.fn().mockRejectedValue({
      code: 'web_auth_required',
      message: '此登录方式需要网页验证，请点击网页授权完成登录'
    })
  });
  render(<CloudManager provider={provider} client={client} initialAccount={{ id: 'account-1', displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByRole('tree', { name: `${label}文件` });
  await user.click(screen.getByText('报告.pdf'));
  await user.click(screen.getByRole('button', { name: '下载' }));

  expect(await screen.findByRole('button', { name: '重新网页授权' })).toBeVisible();
  await user.click(screen.getByRole('button', { name: '重新网页授权' }));
  expect(client.beginCloudAuthorization).toHaveBeenCalledWith(provider);
  await act(async () => { client.emitAuthorization({ id: 'account-1', displayName: 'alice', provider, authMethod: 'web' }); });
  expect(await screen.findByRole('region', { name: `${label}文件管理器` })).toBeVisible();
  expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
});

it('reauthorizes from the operation error and resumes the exact recycle-bin action once', async () => {
  const user = userEvent.setup();
  const recycleBinId = cloudRecycleBinDirectoryId.pikpak;
  const deleteCloudFiles = vi.fn()
    .mockRejectedValueOnce({ code: 'web_auth_required', message: '永久删除需要网页验证' })
    .mockResolvedValueOnce(undefined);
  const client = createCloudFake({
    deleteCloudFiles,
    listCloudDirectory: vi.fn().mockImplementation((_provider: CloudProvider, parentId = '0') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [{ id: 'deleted-file', parentId: recycleBinId, name: '待永久删除.txt', kind: 'file', size: 12 }] }
      : { id: parentId, items: [] }))
  });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);

  await user.dblClick(await screen.findByText('回收站'));
  await user.click(await screen.findByText('待永久删除.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  await user.click(within(screen.getByRole('alertdialog', { name: '确认永久删除' })).getByRole('button', { name: '永久删除', exact: true }));

  const continueButton = await screen.findByRole('button', { name: '打开 PikPak 网盘 授权并继续永久删除' });
  expect(deleteCloudFiles).toHaveBeenCalledTimes(1);
  await user.click(continueButton);
  await act(async () => { client.emitAuthorization({ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }); });

  await waitFor(() => expect(deleteCloudFiles).toHaveBeenCalledTimes(2));
  expect(deleteCloudFiles).toHaveBeenNthCalledWith(2, 'pikpak', ['deleted-file']);
  expect(client.listCloudDirectory).toHaveBeenLastCalledWith('pikpak', recycleBinId);
  expect(await screen.findByRole('status')).toHaveTextContent('永久删除成功');
});

it('does not resume a protected operation when authorization returns another account', async () => {
  const user = userEvent.setup();
  const downloadCloudFile = vi.fn().mockRejectedValue({ code: 'web_auth_required', message: '下载需要网页验证' });
  const client = createCloudFake({ downloadCloudFile });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);

  await user.click(await screen.findByText('报告.pdf'));
  await user.click(screen.getByRole('button', { name: '下载' }));
  await user.click(await screen.findByRole('button', { name: '打开 PikPak 网盘 授权并继续下载' }));
  await act(async () => { client.emitAuthorization({ id: 'p-2', displayName: 'bob', provider: 'pikpak', authMethod: 'web' }); });

  expect(downloadCloudFile).toHaveBeenCalledTimes(1);
  expect(await screen.findByRole('alert')).toHaveTextContent('授权账号与当前会话不一致');
  expect(screen.getByRole('button', { name: '打开 PikPak 网盘 授权并继续下载' })).toBeVisible();
});

it('opens PikPak operation verification without reauthorizing or replaying permanent deletion', async () => {
  const user = userEvent.setup();
  const recycleBinId = cloudRecycleBinDirectoryId.pikpak;
  const deleteCloudFiles = vi.fn().mockRejectedValue({
    code: 'pikpak_operation_verification_required',
    message: 'PikPak 读取文件信息需要官方操作验证。验证完成后请返回重试。',
  });
  const openCloudOperationVerification = vi.fn().mockResolvedValue(undefined);
  const client = createCloudFake({
    deleteCloudFiles,
    listCloudDirectory: vi.fn().mockImplementation((_provider: CloudProvider, parentId = '0') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [{ id: 'deleted-file', parentId: recycleBinId, name: '待验证.txt', kind: 'file', size: 12 }] }
      : { id: parentId, items: [] })),
  });
  Object.assign(client, { openCloudOperationVerification });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  await user.dblClick(await screen.findByText('回收站'));
  await user.click(await screen.findByText('待验证.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  await user.click(within(screen.getByRole('alertdialog', { name: '确认永久删除' })).getByRole('button', { name: '永久删除', exact: true }));
  await user.click(await screen.findByRole('button', { name: '打开 PikPak 操作验证' }));
  expect(openCloudOperationVerification).toHaveBeenCalledWith('pikpak', 'p-1');
  expect(client.beginCloudAuthorization).not.toHaveBeenCalled();
  expect(deleteCloudFiles).toHaveBeenCalledTimes(1);
  expect(screen.queryByRole('button', { name: '重新网页授权' })).not.toBeInTheDocument();
  expect(screen.getByRole('alert')).toHaveTextContent('返回后重新执行原操作');
  expect(screen.getByText('待验证.txt')).toBeVisible();
});

it('keeps operation verification available when deletion succeeds but directory refresh is challenged', async () => {
  const user = userEvent.setup();
  let deleted = false;
  const client = createCloudFake({
    deleteCloudFiles: vi.fn().mockImplementation(async () => { deleted = true; }),
    listCloudDirectory: vi.fn().mockImplementation(async (_provider: CloudProvider, parentId = '0') => {
      if (deleted) throw { code: 'pikpak_operation_verification_required', message: '读取文件列表需要官方操作验证' };
      return { id: parentId, items: parentId === cloudRecycleBinDirectoryId.pikpak
        ? [{ id: 'file', parentId, name: '已删除.txt', kind: 'file', size: 12 }] : [] };
    }),
  });
  Object.assign(client, { openCloudOperationVerification: vi.fn().mockResolvedValue(undefined) });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  await user.dblClick(await screen.findByText('回收站'));
  await user.click(await screen.findByText('已删除.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  await user.click(within(screen.getByRole('alertdialog', { name: '确认永久删除' })).getByRole('button', { name: '永久删除', exact: true }));
  await waitFor(() => expect(deleted).toBe(true));
  expect((await screen.findAllByRole('button', { name: '打开 PikPak 操作验证' })).length).toBeGreaterThan(0);
  expect(screen.queryByText('永久删除成功')).not.toBeInTheDocument();
});

it('unblocks controls when deletion succeeds but refreshing the directory loses the network', async () => {
  const user = userEvent.setup();
  let deleted = false;
  const client = createCloudFake({
    deleteCloudFiles: vi.fn().mockImplementation(async () => { deleted = true; }),
    listCloudDirectory: vi.fn().mockImplementation(async (_provider: CloudProvider, parentId = '0') => {
      if (deleted) throw { code: 'network_error', message: '网络中断' };
      return { id: parentId, items: parentId === cloudRecycleBinDirectoryId.pikpak
        ? [{ id: 'file', parentId, name: '网络测试.txt', kind: 'file', size: 12 }] : [] };
    }),
  });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  await user.dblClick(await screen.findByText('回收站'));
  await user.click(await screen.findByText('网络测试.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  await user.click(within(screen.getByRole('alertdialog', { name: '确认永久删除' })).getByRole('button', { name: '永久删除', exact: true }));
  await screen.findByText('网络中断');
  expect(screen.getByRole('button', { name: '刷新' })).toBeEnabled();
  expect(screen.getByRole('button', { name: '退出授权' })).toBeEnabled();
  expect(screen.queryByText('永久删除进行中…')).not.toBeInTheDocument();
});

it.each(['provider_error', 'invalid_response', 'credentials_expired'])(
  'stops Xunlei authorization polling after %s until the user starts it again', async (code) => {
    vi.useFakeTimers();
    try {
      const client = createCloudFake({ completeCloudAuthorization: vi.fn()
        .mockRejectedValueOnce({ code, message: '迅雷授权验证失败，请重新授权。' })
        .mockResolvedValue({ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }) });
      const onAccountAuthorized = vi.fn();
      render(<CloudManager provider="xunlei" client={client} authorizationOnly onAccountAuthorized={onAccountAuthorized} />);

      await act(async () => { await Promise.resolve(); });
      fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
      await act(async () => { await Promise.resolve(); });
      await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
      await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });

      expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(1);
      expect(onAccountAuthorized).not.toHaveBeenCalled();
      expect(screen.getByRole('status')).toHaveTextContent('迅雷授权验证失败，请重新授权。');
      expect(screen.queryByRole('button', { name: '我已完成授权' })).not.toBeInTheDocument();
      fireEvent.click(screen.getByRole('button', { name: '使用网页授权' }));
      await act(async () => { await Promise.resolve(); });
      await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
      expect(client.beginCloudAuthorization).toHaveBeenCalledTimes(2);
      expect(onAccountAuthorized).toHaveBeenCalledWith({ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' });
    } finally {
      vi.useRealTimers();
    }
  }
);

it('preserves the Xunlei account, directory and rejection when renewed authorization stops', async () => {
  vi.useFakeTimers();
  try {
    const client = createCloudFake({
      downloadCloudFile: vi.fn().mockRejectedValue({ code: 'web_auth_required', message: '需要网页验证' }),
      completeCloudAuthorization: vi.fn().mockRejectedValue({ code: 'provider_error', message: '迅雷拒绝了本次授权验证。' })
    });
    const onAccountLoggedOut = vi.fn();
    render(<CloudManager provider="xunlei" client={client} initialAccount={{ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }} restoreOnMount={false} showSessionActions={false} onAccountLoggedOut={onAccountLoggedOut} />);
    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByText('报告.pdf'));
    fireEvent.click(screen.getByRole('button', { name: '下载' }));
    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByRole('button', { name: '重新网页授权' }));
    await act(async () => { await Promise.resolve(); });
    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });

    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(1);
    expect(screen.getByRole('region', { name: '迅雷网盘文件管理器' })).toBeVisible();
    expect(screen.getByText('报告.pdf')).toBeVisible();
    expect(screen.getByRole('alert')).toHaveTextContent('迅雷拒绝了本次授权验证。');
    expect(screen.getByRole('button', { name: '重新网页授权' })).toBeVisible();
    expect(client.logoutCloud).not.toHaveBeenCalled();
    expect(onAccountLoggedOut).not.toHaveBeenCalled();
  } finally {
    vi.useRealTimers();
  }
});

it.each(['web_auth_required', 'credentials_expired'])('recovers Xunlei initial directory %s with session actions hidden', async (code) => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudDirectory: vi.fn()
    .mockRejectedValueOnce({ code, message: '网盘需要重新授权' })
    .mockResolvedValue({ id: '0', items: [{ id: 'file-2', parentId: '0', name: '恢复后的文件.txt', kind: 'file' }] }) });
  render(<CloudManager provider="xunlei" client={client} initialAccount={{ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }} restoreOnMount={false} showSessionActions={false} />);

  await user.click(await screen.findByRole('button', { name: '重新网页授权' }));
  expect(client.beginCloudAuthorization).toHaveBeenCalledWith('xunlei');
  await act(async () => { client.emitAuthorization({ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }); });
  expect(screen.getByRole('region', { name: '迅雷网盘文件管理器' })).toBeVisible();
  expect(await screen.findByText('恢复后的文件.txt', {}, { timeout: 2_000 })).toBeVisible();
  expect(screen.queryByRole('button', { name: '重新网页授权' })).not.toBeInTheDocument();
});

it.each(['web_auth_required', 'credentials_expired'])('preserves the Xunlei directory after refresh returns %s', async (code) => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="xunlei" client={client} initialAccount={{ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }} restoreOnMount={false} showSessionActions={false} />);

  await screen.findByText('报告.pdf');
  client.listCloudDirectory.mockRejectedValueOnce({ code, message: '网盘需要重新授权' });
  await user.click(screen.getByRole('button', { name: '刷新' }));
  expect(await screen.findByRole('button', { name: '重新网页授权' })).toBeVisible();
  expect(screen.getByText('报告.pdf')).toBeVisible();
  expect(screen.getByRole('tree', { name: '迅雷网盘文件' })).toBeVisible();
  expect(screen.getByRole('alert')).toHaveTextContent('读取目录需要网页验证');
});

it('resumes loading the requested nested directory instead of returning to the root after authorization', async () => {
  const user = userEvent.setup();
  const listCloudDirectory = vi.fn()
    .mockResolvedValueOnce({ id: '0', items: [{ id: 'folder-1', parentId: '0', name: '资料', kind: 'folder' }] })
    .mockRejectedValueOnce({ code: 'web_auth_required', message: '读取资料目录需要验证' })
    .mockResolvedValueOnce({ id: 'folder-1', items: [{ id: 'nested-file', parentId: 'folder-1', name: '内容.txt', kind: 'file' }] });
  const client = createCloudFake({ listCloudDirectory });
  render(<CloudManager provider="uc" client={client} initialAccount={{ id: 'uc-1', displayName: 'alice', provider: 'uc', authMethod: 'web' }} restoreOnMount={false} />);

  await user.dblClick(await screen.findByText('资料'));
  await user.click(await screen.findByRole('button', { name: '打开 UC 网盘 授权并继续加载目录' }));
  await act(async () => { client.emitAuthorization({ id: 'uc-1', displayName: 'alice', provider: 'uc', authMethod: 'web' }); });

  expect(await screen.findByText('内容.txt')).toBeVisible();
  expect(screen.getByRole('button', { name: '资料' })).toHaveAttribute('aria-current', 'page');
  expect(listCloudDirectory).toHaveBeenNthCalledWith(3, 'uc', 'folder-1');
});

it.each(['web_auth_required', 'credentials_expired'])('keeps the Xunlei account and directory during pending renewal after %s', async (code) => {
  vi.useFakeTimers();
  try {
    const client = createCloudFake({
      downloadCloudFile: vi.fn().mockRejectedValueOnce({ code, message: '网盘需要重新授权' }).mockResolvedValue(undefined),
      completeCloudAuthorization: vi.fn()
        .mockRejectedValueOnce({ code: 'authorization_pending', message: '正在等待登录' })
        .mockResolvedValue({ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' })
    });
    const onAccountLoggedOut = vi.fn();
    render(<CloudManager provider="xunlei" client={client} initialAccount={{ id: 'account-1', displayName: 'alice', provider: 'xunlei', authMethod: 'web' }} restoreOnMount={false} showSessionActions={false} onAccountLoggedOut={onAccountLoggedOut} />);
    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByText('报告.pdf'));
    fireEvent.click(screen.getByRole('button', { name: '下载' }));
    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByRole('button', { name: '重新网页授权' }));
    await act(async () => { await Promise.resolve(); });
    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });

    expect(screen.getByRole('region', { name: '迅雷网盘文件管理器' })).toBeVisible();
    expect(screen.getByText('报告.pdf')).toBeVisible();
    expect(client.logoutCloud).not.toHaveBeenCalled();
    expect(onAccountLoggedOut).not.toHaveBeenCalled();

    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(client.completeCloudAuthorization).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole('button', { name: '重新网页授权' })).not.toBeInTheDocument();
    expect(screen.getByRole('tree', { name: '迅雷网盘文件' })).toBeVisible();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  } finally {
    vi.useRealTimers();
  }
});

it('surfaces the message when opening a web authorization window fails', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ beginCloudAuthorization: vi.fn().mockRejectedValue({ code: 'authorization_unavailable', message: '无法打开授权窗口' }) });
  render(<CloudManager provider="pikpak" client={client} />);

  await user.click(await screen.findByRole('button', { name: '使用网页授权' }));

  expect(await screen.findByRole('status')).toHaveTextContent('无法打开授权窗口');
});

it('prevents duplicate directory submissions while the first operation is pending', async () => {
  const user = userEvent.setup();
  let finish: (() => void) | undefined;
  const pending = new Promise<void>((resolve) => { finish = resolve; });
  const client = createCloudFake({ createCloudDirectory: vi.fn().mockReturnValue(pending) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.click(screen.getByRole('button', { name: '新建目录' }));
  await user.type(screen.getByLabelText('目录名称'), '资料');
  const submit = screen.getByRole('button', { name: '确认新建目录' });
  fireEvent.click(submit);
  fireEvent.click(submit);

  expect(client.createCloudDirectory).toHaveBeenCalledTimes(1);
  expect(submit).toBeDisabled();
  await act(async () => { finish?.(); await pending; });
  expect(await screen.findByRole('status')).toHaveTextContent('新建目录成功');
});

it('treats closing the upload picker as a cancellation instead of a success', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ pickAndUploadFiles: vi.fn().mockResolvedValue({ uploaded: [], failures: [] }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.click(screen.getByRole('button', { name: '上传' }));
  await user.click(within(await screen.findByRole('dialog', { name: '选择上传内容' })).getByRole('button', { name: '取消' }));
  expect(client.uploadCloudFiles).not.toHaveBeenCalled();
  expect(client.pickAndUploadFiles).not.toHaveBeenCalled();
  expect(screen.queryByText(/上传成功|已上传/)).not.toBeInTheDocument();
});

it('keeps selected upload paths and retries only authorization-failed files after reauthorization', async () => {
  const user = userEvent.setup();
  const uploadCloudFiles = vi.fn()
    .mockResolvedValueOnce({
      uploaded: [{ id: 'uploaded-a', parentId: '0', name: 'a.txt', kind: 'file' }],
      failures: [{ inputIndex: 1, itemName: 'b.txt', code: 'web_auth_required', message: '上传需要网页验证' }]
    })
    .mockResolvedValueOnce({
      uploaded: [{ id: 'uploaded-b', parentId: '0', name: 'b.txt', kind: 'file' }],
      failures: []
    });
  const client = createCloudFake({
    uploadCloudFiles,
    listCloudDirectory: vi.fn().mockResolvedValue({ id: '0', items: [
      { id: 'uploaded-a', parentId: '0', name: 'a.txt', kind: 'file' },
      { id: 'uploaded-b', parentId: '0', name: 'b.txt', kind: 'file' },
    ] })
  });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByRole('tree', { name: 'PikPak 网盘文件' });
  await user.click(screen.getByRole('button', { name: '上传' }));
  await confirmAllLocalUploads(user);
  await user.click(await screen.findByRole('button', { name: '打开 PikPak 网盘 授权并继续上传' }));
  await act(async () => { client.emitAuthorization({ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }); });

  await waitFor(() => expect(uploadCloudFiles).toHaveBeenCalledTimes(2));
  expect(uploadCloudFiles).toHaveBeenNthCalledWith(1, 'pikpak', '0', ['C:/a.txt', 'C:/b.txt'], expect.any(Function));
  expect(uploadCloudFiles).toHaveBeenNthCalledWith(2, 'pikpak', '0', ['C:/b.txt'], expect.any(Function));
  expect(client.listLocalUploadEntries).toHaveBeenCalledTimes(1);
});

it.each([
  ['部分成功', [{ id: 'uploaded', parentId: '0', name: '成功.txt', kind: 'file' as const }]],
  ['全部失败', []]
])('refreshes after an upload %s and reports the retained failures', async (_label, uploaded) => {
  const user = userEvent.setup();
  const listCloudDirectory = vi.fn()
    .mockResolvedValueOnce({ id: '0', items: [] })
    .mockResolvedValueOnce({ id: '0', items: uploaded });
  const client = createCloudFake({
    listCloudDirectory,
    uploadCloudFiles: vi.fn().mockResolvedValue({
      uploaded,
      failures: [{ itemName: '失败.txt', code: 'network_error', message: '网络中断' }]
    })
  });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.click(screen.getByRole('button', { name: '上传' }));

  await confirmAllLocalUploads(user);
  expect(await screen.findByRole('alert')).toHaveTextContent(`成功 ${uploaded.length} 项，失败 1 项：失败.txt：网络中断`);
  expect(listCloudDirectory).toHaveBeenCalledTimes(2);
});

it('opens Baidu recycle-bin verification and waits for an explicit retry of permanent deletion', async () => {
  const user = userEvent.setup();
  const recycleBinId = cloudRecycleBinDirectoryId.baidu;
  const deleteCloudFiles = vi.fn()
    .mockRejectedValueOnce({ code: 'baidu_operation_verification_required', message: 'baidu_operation_verification_required' })
    .mockResolvedValueOnce(undefined);
  const openCloudOperationVerification = vi.fn().mockResolvedValue(undefined);
  const completeCloudOperationVerification = vi.fn().mockResolvedValue({ id: 'b-1', displayName: 'alice', provider: 'baidu', authMethod: 'web' });
  const client = createCloudFake({
    deleteCloudFiles,
    openCloudOperationVerification,
    completeCloudOperationVerification,
    listCloudDirectory: vi.fn().mockImplementation((_provider: CloudProvider, parentId = '/') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [{ id: '987', parentId: recycleBinId, name: '待验证.txt', kind: 'file', size: 12 }] }
      : { id: parentId, items: [] })),
  });
  render(<CloudManager provider="baidu" client={client} initialAccount={{ id: 'b-1', displayName: 'alice', provider: 'baidu', authMethod: 'web' }} restoreOnMount={false} />);

  await user.dblClick(await screen.findByText('回收站'));
  await user.click(await screen.findByText('待验证.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  await user.click(within(screen.getByRole('alertdialog', { name: '确认永久删除' })).getByRole('button', { name: '永久删除', exact: true }));

  expect(await screen.findByRole('alert')).toHaveTextContent('百度网盘需要安全验证');
  await user.click(screen.getByRole('button', { name: '打开百度网盘官方回收站验证' }));
  expect(openCloudOperationVerification).toHaveBeenCalledWith('baidu', 'b-1', document.documentElement.lang);
  expect(client.beginCloudAuthorization).not.toHaveBeenCalled();
  expect(deleteCloudFiles).toHaveBeenCalledTimes(1);

  await user.click(screen.getByRole('button', { name: '我已完成验证' }));
  expect(completeCloudOperationVerification).toHaveBeenCalledWith('baidu', 'b-1');
  expect(deleteCloudFiles).toHaveBeenCalledTimes(1);
  expect(screen.getByText('待验证.txt')).toBeVisible();

  await user.click(screen.getByRole('button', { name: '永久删除' }));
  await user.click(within(screen.getByRole('alertdialog', { name: '确认永久删除' })).getByRole('button', { name: '永久删除', exact: true }));
  await waitFor(() => expect(deleteCloudFiles).toHaveBeenCalledTimes(2));
  expect(deleteCloudFiles).toHaveBeenNthCalledWith(2, 'baidu', ['987']);
});

it('localizes a Baidu verification account mismatch without retrying permanent deletion', async () => {
  const user = userEvent.setup();
  const recycleBinId = cloudRecycleBinDirectoryId.baidu;
  const deleteCloudFiles = vi.fn().mockRejectedValueOnce({ code: 'baidu_operation_verification_required' });
  const completeCloudOperationVerification = vi.fn().mockRejectedValue({ code: 'account_mismatch', message: '验证账号不一致' });
  const client = createCloudFake({
    deleteCloudFiles,
    openCloudOperationVerification: vi.fn().mockResolvedValue(undefined),
    completeCloudOperationVerification,
    listCloudDirectory: vi.fn().mockImplementation((_provider: CloudProvider, parentId = '/') => Promise.resolve(parentId === recycleBinId
      ? { id: recycleBinId, items: [{ id: '987', parentId: recycleBinId, name: 'verify.txt', kind: 'file', size: 12 }] }
      : { id: parentId, items: [] })),
  });
  render(<CloudManager provider="baidu" client={client} initialAccount={{ id: 'b-1', displayName: 'alice', provider: 'baidu', authMethod: 'web' }} restoreOnMount={false} />);

  await user.dblClick(await screen.findByText('回收站'));
  await user.click(await screen.findByText('verify.txt'));
  await user.click(screen.getByRole('button', { name: '永久删除' }));
  await user.click(within(screen.getByRole('alertdialog', { name: '确认永久删除' })).getByRole('button', { name: '永久删除', exact: true }));
  await user.click(await screen.findByRole('button', { name: '打开百度网盘官方回收站验证' }));
  await user.click(screen.getByRole('button', { name: '我已完成验证' }));

  expect(await screen.findByRole('alert')).toHaveTextContent('验证账号与当前会话不一致。请使用原账号完成验证。');
  expect(screen.getByRole('alert')).not.toHaveTextContent('验证账号不一致');
  expect(deleteCloudFiles).toHaveBeenCalledTimes(1);
});

it.each(['quark', 'pikpak', 'baidu', 'tianyi', 'aliyun', 'xunlei', 'uc'] as const)('retries the server-backed directory listing until an uploaded %s file is visible', async (provider) => {
  const user = userEvent.setup();
  const rootId = cloudCatalog.find((descriptor) => descriptor.provider === provider)!.rootDirectoryId;
  const uploaded = { id: `${provider}-uploaded`, parentId: rootId, name: '已确认.txt', kind: 'file' as const };
  const listCloudDirectory = vi.fn()
    .mockResolvedValueOnce({ id: rootId, items: [] })
    .mockResolvedValueOnce({ id: rootId, items: [] })
    .mockResolvedValueOnce({ id: rootId, items: [uploaded] });
  const client = createCloudFake({
    listCloudDirectory,
    uploadCloudFiles: vi.fn().mockResolvedValue({ uploaded: [uploaded], failures: [] })
  });
  render(<CloudManager provider={provider} client={client} initialAccount={{ id: `${provider}-1`, displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传' }));
  await confirmAllLocalUploads(user);

  expect(await screen.findByText('已确认.txt')).toBeVisible();
  expect(listCloudDirectory).toHaveBeenCalledTimes(3);
});

it('confirms redirected PikPak uploads in the actual destination and displays that folder', async () => {
  const user = userEvent.setup();
  const folder = { id: 'server-upload-folder', parentId: '0', name: 'My Upload', kind: 'folder' as const };
  const uploaded = { id: 'uploaded', parentId: folder.id, name: '上传成功.txt', kind: 'file' as const };
  let destinationReads = 0;
  const listCloudDirectory = vi.fn(async (_provider, parentId) => parentId === '0'
    ? { id: '0', items: [folder] }
    : { id: folder.id, items: ++destinationReads > 1 ? [uploaded] : [] });
  const client = createCloudFake({ listCloudDirectory, uploadCloudFiles: vi.fn().mockResolvedValue({ uploaded: [uploaded], failures: [] }) });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传' }));
  await confirmAllLocalUploads(user);
  expect(await screen.findByText('上传成功.txt')).toBeVisible();
  expect(await screen.findByText('已上传 1 项；已转到上传目录：My Upload')).toBeVisible();
  expect(screen.queryByText(/服务端目录尚未同步/)).not.toBeInTheDocument();
  expect(destinationReads).toBe(2);
  expect(listCloudDirectory).toHaveBeenCalledTimes(4);
});

it.each([false, true])('confirms directory upload roots without mistaking descendants for redirected uploads (mixed=%s)', async (mixed) => {
  const user = userEvent.setup();
  const folder = { id: 'new-folder', parentId: '0', name: '新上传目录', kind: 'folder' as const };
  const child = { id: 'child', parentId: folder.id, name: '子文件.txt', kind: 'file' as const };
  const redirected = { id: 'redirected', parentId: 'server-upload-folder', name: '单文件.txt', kind: 'file' as const };
  const listCloudDirectory = vi.fn(async (_provider, parentId) => ({ id: parentId, items: parentId === '0' ? [folder] : [redirected] }));
  const client = createCloudFake({ listCloudDirectory, uploadCloudFiles: vi.fn().mockResolvedValue({ uploaded: mixed ? [folder, child, redirected] : [folder, child], failures: [] }) });
  render(<CloudManager provider="pikpak" client={client} initialAccount={{ id: 'p-1', displayName: 'alice', provider: 'pikpak', authMethod: 'web' }} restoreOnMount={false} />);
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传' }));
  await confirmAllLocalUploads(user);
  await waitFor(() => expect(listCloudDirectory).toHaveBeenCalledTimes(mixed ? 3 : 2));
  expect(screen.getByText('新上传目录')).toBeVisible();
  expect(listCloudDirectory).not.toHaveBeenCalledWith('pikpak', folder.id);
  expect(screen.queryByText(/服务端目录尚未同步/)).not.toBeInTheDocument();
  if (mixed) expect(listCloudDirectory).toHaveBeenCalledWith('pikpak', redirected.parentId);
});

it('refreshes an upload outcome before offering inline reauthorization for expired credentials', async () => {
  const user = userEvent.setup();
  const listCloudDirectory = vi.fn()
    .mockResolvedValueOnce({ id: '0', items: [] })
    .mockResolvedValueOnce({ id: '0', items: [{ id: 'uploaded', parentId: '0', name: '已完成.txt', kind: 'file' }] });
  const client = createCloudFake({
    listCloudDirectory,
    uploadCloudFiles: vi.fn().mockResolvedValue({
      uploaded: [{ id: 'uploaded', parentId: '0', name: '已完成.txt', kind: 'file' }],
      failures: [{ itemName: '待重试.txt', code: 'credentials_expired', message: '登录已过期' }]
    })
  });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.click(screen.getByRole('button', { name: '上传' }));

  await confirmAllLocalUploads(user);
  expect(await screen.findByRole('alert')).toHaveTextContent('上传需要网页验证');
  expect(screen.getByRole('alert')).toHaveTextContent('登录已过期');
  expect(screen.getByRole('button', { name: '打开 夸克网盘 授权并继续上传' })).toBeVisible();
  expect(listCloudDirectory).toHaveBeenCalledTimes(2);
});

it('opens the same mixed picker from the file-area upload context command', async () => {
  const user = userEvent.setup();
  const client = createCloudFake();
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  const tree = await screen.findByRole('tree', { name: '夸克网盘文件' });
  fireEvent.contextMenu(tree, { clientX: 100, clientY: 100 });
  await user.click(screen.getByRole('menuitem', { name: '上传' }));

  expect(await screen.findByRole('dialog', { name: '选择上传内容' })).toBeVisible();
  await confirmAllLocalUploads(user);
  expect(client.uploadCloudFiles).toHaveBeenCalledWith('quark', '0', ['C:/a.txt', 'C:/b.txt'], expect.any(Function));
});

it('automatically completes a renewed web authorization after credentials expire', async () => {
  const user = userEvent.setup();
  const client = createCloudFake({ listCloudDirectory: vi.fn()
    .mockRejectedValueOnce(Object.assign(new Error('会话失效'), { code: 'credentials_expired' }))
    .mockResolvedValue({ id: '0', items: [{ id: 'file-1', parentId: '0', name: '恢复后的文件.txt', kind: 'file', size: 12 }] }) });
  render(<CloudManager provider="quark" client={client} initialAccount={{ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }} restoreOnMount={false} />);

  expect(await screen.findByRole('alert')).toHaveTextContent('会话失效');
  await user.click(await screen.findByRole('button', { name: '打开 夸克网盘 授权并继续加载目录' }));
  await act(async () => { client.emitAuthorization({ id: 'q-1', displayName: 'alice', provider: 'quark', authMethod: 'web' }); });

  expect(await screen.findByRole('tree', { name: '夸克网盘文件' }, { timeout: 2_000 })).toBeVisible();
  expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
});

it.each([
  ['baidu', 'https://pan.baidu.com/s/test', 'https://pan.baidu.com/s/test?pwd=a7B2'],
  ['xunlei', 'https://pan.xunlei.com/s/test?pwd=a7B2', 'https://pan.xunlei.com/s/test?pwd=a7B2'],
  ['uc', 'https://drive.uc.cn/s/test', 'https://drive.uc.cn/s/test'],
] as const)('copies only a usable URL from the %s share list', async (provider, url, expected) => {
  const user = userEvent.setup();
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
  const client = createCloudFake({ listCloudShares: vi.fn().mockResolvedValue([
    { id: 'copy-test', url, name: '复制测试.pdf', kind: 'file', sourceFileIds: ['file-1'], passCode: 'a7B2' }
  ]) });
  render(<CloudManager provider={provider} client={client} initialAccount={{ id: 'test', displayName: 'alice', provider, authMethod: 'web' }} restoreOnMount={false} />);
  await user.dblClick(await screen.findByText('我的分享'));
  fireEvent.contextMenu(await screen.findByText('复制测试.pdf'));
  await user.click(screen.getByRole('menuitem', { name: '复制分享链接' }));
  await waitFor(() => expect(writeText).toHaveBeenCalledWith(expected));
  expect(new URL(writeText.mock.calls[0][0]).href).toBe(expected);
});
