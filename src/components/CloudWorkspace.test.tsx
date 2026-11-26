import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { useState } from 'react';
import { CloudWorkspace as WorkspaceContent, type CloudWorkspacePage, type CloudWorkspaceProps } from './CloudWorkspace';
import { CloudNavigation } from './CloudNavigation';
import type { ProviderDescriptor } from '../lib/cloudTypes';

function CloudWorkspace(props: Omit<CloudWorkspaceProps, 'activePage' | 'onPageChange' | 'onProvidersChange'>) {
  const [page, setPage] = useState<CloudWorkspacePage>('management');
  const [providers, setProviders] = useState<readonly ProviderDescriptor[]>([]);
  return <><nav aria-label="主导航"><CloudNavigation providers={providers} activePage={page} onSelect={setPage} icon={null} /></nav>
    <WorkspaceContent {...props} activePage={page} onPageChange={setPage} onProvidersChange={setProviders} /></>;
}
import { cloudCatalog } from '../test/cloudCatalog';

it.each(['success', 'failure'] as const)('retains in-progress uploads across providers but clears the %s result on a later provider switch', async (outcome) => {
  const user = userEvent.setup();
  let finish!: (value: any) => void;
  let uploaded = false;
  const client = {
    ...createClient({
      quark: { id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' },
      pikpak: { id: 'p', displayName: 'bob', provider: 'pikpak', authMethod: 'web' },
    }),
    listLocalUploadEntries: vi.fn().mockResolvedValue({ path: 'C:/Uploads', parentPath: 'C:/', roots: [], entries: [{ path: 'C:/Uploads/a.txt', name: 'a.txt', kind: 'file', size: 1024 }] }),
    listCloudDirectory: vi.fn((provider: string, parentId = '0') => Promise.resolve({ id: parentId, items: [
      { id: `${provider}-file`, parentId, name: `${provider}-文件.txt`, kind: 'file', size: 10, modifiedAt: '刚刚' },
      ...(uploaded && provider === 'quark' ? [{ id: 'new-file', parentId, name: 'a.txt', kind: 'file' }] : []),
    ] })),
    uploadCloudFiles: vi.fn(() => new Promise<any>(resolve => { finish = resolve; })),
  };
  const view = render(<div><CloudWorkspace client={client} /></div>);
  await user.click(await screen.findByRole('button', { name: '夸克网盘' }));
  await screen.findByRole('tree', { name: '夸克网盘文件' });
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await user.click(await screen.findByRole('checkbox', { name: '选择 a.txt' }));
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  expect(await screen.findByText('正在准备文件')).toBeVisible();
  await user.click(screen.getByRole('button', { name: 'PikPak 网盘' }));
  await screen.findByRole('tree', { name: 'PikPak 网盘文件' });
  const inactiveUpload = screen.queryByText('正在准备文件');
  if (inactiveUpload) expect(inactiveUpload).not.toBeVisible();
  await user.click(screen.getByRole('button', { name: '夸克网盘' }));
  expect(await screen.findByText('正在准备文件')).toBeVisible();
  expect(screen.getByRole('button', { name: '上传', exact: true })).toBeDisabled();
  view.rerender(<div hidden><CloudWorkspace client={client} /></div>);
  uploaded = outcome === 'success';
  await act(async () => { finish(outcome === 'success'
    ? { uploaded: [{ id: 'new-file', name: 'a.txt', kind: 'file', parentId: '0' }], failures: [] }
    : { uploaded: [], failures: [{ inputIndex: 0, itemName: 'a.txt', code: 'network_error', message: '网络中断' }] }); });
  view.rerender(<div><CloudWorkspace client={client} /></div>);
  const message = outcome === 'success' ? '已上传 1 项' : '上传完成：成功 0 项，失败 1 项：a.txt：网络中断';
  expect(await screen.findByText(message)).toBeVisible();
  await waitFor(() => expect(screen.getByRole('button', { name: '刷新' })).toBeEnabled());
  await user.click(screen.getByRole('button', { name: 'PikPak 网盘' }));
  await user.click(screen.getByRole('button', { name: '夸克网盘' }));
  await waitFor(() => expect(screen.queryByText(message)).not.toBeInTheDocument());
  await user.click(screen.getByRole('button', { name: '刷新' }));
  expect(screen.queryByText(message)).not.toBeInTheDocument();
  expect(client.uploadCloudFiles).toHaveBeenCalledTimes(1);
  await user.click(screen.getByRole('button', { name: '我的网盘' }));
  const card = screen.getByRole('region', { name: '夸克网盘账号授权' });
  await user.click(within(card).getByRole('button', { name: '退出授权' }));
  await user.click(screen.getByRole('button', { name: '确认退出授权' }));
  await waitFor(() => expect(screen.queryByText(message)).not.toBeInTheDocument());
  const authorization = screen.getByRole('region', { name: '夸克网盘账号授权' });
  await user.click(within(authorization).getByRole('button', { name: '使用网页授权' }));
  await act(async () => { client.emitAuthorization({ id: 'different-account', displayName: 'other', provider: 'quark', authMethod: 'web' }); });
  await user.click(screen.getByRole('button', { name: '夸克网盘' }));
  await screen.findByRole('tree', { name: '夸克网盘文件' });
  expect(screen.queryByText(message)).not.toBeInTheDocument();
});

type TestAccount = { id: string; displayName: string; provider: 'quark' | 'pikpak'; authMethod: 'web' | 'password' };

it('keeps paused upload controls scoped to the original provider after navigation', async () => {
  const user = userEvent.setup();
  let receive!: (value: any) => void;
  const client = {
    ...createClient({ quark: { id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }, pikpak: { id: 'p', displayName: 'bob', provider: 'pikpak', authMethod: 'web' } }),
    listLocalUploadEntries: vi.fn().mockResolvedValue({ path: 'C:/', parentPath: null, roots: [], entries: [{ path: 'C:/a.txt', name: 'a.txt', kind: 'file', size: 100 }] }),
    uploadCloudFiles: vi.fn((_p, _d, _paths, callback) => { receive = callback; return new Promise<any>(() => {}); }),
    controlCloudUpload: vi.fn().mockResolvedValue(undefined),
  };
  render(<CloudWorkspace client={client} />);
  await user.click(await screen.findByRole('button', { name: '夸克网盘' }));
  await screen.findByRole('tree');
  await user.click(screen.getByRole('button', { name: '上传', exact: true }));
  await user.click(await screen.findByRole('checkbox', { name: '选择 a.txt' }));
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  await act(async () => { receive({ uploadId: 'quark-upload', provider: 'quark', phase: 'uploading', totalBytes: 100, uploadedBytes: 40, speedBytesPerSecond: 10, totalFiles: 1, completedFiles: 0, failedItems: 0, currentFiles: ['a.txt'], progress: 40 }); });
  await user.click(screen.getByRole('button', { name: '暂停上传' }));
  await user.click(screen.getByRole('button', { name: 'PikPak 网盘' }));
  expect(screen.queryByRole('button', { name: '继续上传' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '取消上传' })).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '夸克网盘' }));
  expect(screen.getByText('上传已暂停：a.txt')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '继续上传' }));
  expect(client.controlCloudUpload.mock.calls).toEqual([['quark', 'quark-upload', 'pause'], ['quark', 'quark-upload', 'resume']]);
});

it('closes portal menus when leaving a cached provider workspace', async () => {
  const user = userEvent.setup();
  const client = createClient({ quark: { id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' } });
  render(<CloudWorkspace client={client} />);
  await user.click(await screen.findByRole('button', { name: '夸克网盘' }));
  fireEvent.contextMenu(await screen.findByText('quark-文件.txt'), { clientX: 100, clientY: 100 });
  expect(screen.getByRole('menu')).toBeVisible();
  fireEvent.click(screen.getByRole('button', { name: '我的网盘' }));
  expect(screen.queryByRole('menu')).not.toBeInTheDocument();
});

it('adds the authorized workspace immediately from the completion event', async () => {
  let receive: ((event: any) => void) | undefined;
  const client = { ...createClient(), getCompletedCloudAuthorization: vi.fn().mockResolvedValue({ id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' }), subscribeCloudAuthorization: vi.fn(async (_provider, callback) => { receive = callback; return vi.fn(); }) };
  client.getCloudProviderCatalog.mockResolvedValue([cloudCatalog[0]]);
  client.getCloudAccount.mockRejectedValue({ code: 'web_auth_required', message: '请在官方网盘完成验证' });
  render(<CloudWorkspace client={client} />);
  fireEvent.click(await screen.findByRole('button', { name: '使用网页授权' }));
  await act(async () => {});
  await act(async () => { receive?.({ provider: 'quark', state: 'Ready', account: { id: 'q', displayName: 'alice', provider: 'quark', authMethod: 'web' } }); });
  expect(screen.getByRole('button', { name: '夸克网盘' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '我已完成授权' })).not.toBeInTheDocument();
  expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
  expect(screen.queryByText(/旧会话错误/)).not.toBeInTheDocument();
});

it('retries only the affected account after an inconclusive startup check without prompting for login', async () => {
  const user = userEvent.setup();
  const client = createClient();
  client.getCloudProviderCatalog.mockResolvedValue([cloudCatalog[0], cloudCatalog[1]]);
  client.getCloudAccount.mockImplementation((provider) => Promise.reject(provider === 'quark'
    ? new Error('网页验证服务连接中断')
    : { code: 'credential_storage_error', message: '凭据存储暂不可用' }));
  render(<CloudWorkspace client={client} />);
  await screen.findByText('网页验证服务连接中断');
  const quarkCard = screen.getByRole('region', { name: '夸克网盘账号恢复' });
  const pikpakCard = screen.getByRole('region', { name: 'PikPak 网盘账号恢复' });
  expect(within(pikpakCard).getByText('凭据存储暂不可用')).toBeVisible();
  expect(screen.queryByText('授权过期，请重新登录')).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '使用网页授权' })).not.toBeInTheDocument();
  expect(screen.queryByLabelText('密码')).not.toBeInTheDocument();
  client.getCloudAccount.mockResolvedValueOnce({ id: 'q1', displayName: 'alice', provider: 'quark', authMethod: 'web' });
  await user.click(within(quarkCard).getByRole('button', { name: '重试' }));
  expect(await screen.findByRole('button', { name: '夸克网盘' })).toBeVisible();
  expect(within(pikpakCard).getByText('凭据存储暂不可用')).toBeVisible();
  expect(client.getCloudAccount).toHaveBeenCalledTimes(3);
  expect(client.getCloudAccount).toHaveBeenLastCalledWith('quark');
  expect(client.loginCloudWithPassword).not.toHaveBeenCalled();
  expect(client.beginCloudAuthorization).not.toHaveBeenCalled();
});

it.each(['credentials_expired', 'web_auth_required'])('offers the specified authorization action for startup %s', async code => {
  const client = createClient();
  client.getCloudProviderCatalog.mockResolvedValue([cloudCatalog[1]]);
  client.getCloudAccount.mockRejectedValue({ code });
  render(<CloudWorkspace client={client} />);
  const card = await screen.findByRole('region', { name: 'PikPak 网盘账号授权' });
  expect(within(card).getByRole('button', { name: '使用网页授权' })).toBeVisible();
  if (code === 'credentials_expired') {
    expect(within(card).getByText('授权过期，请重新登录')).toBeVisible();
  } else {
    expect(within(card).queryByLabelText('密码')).not.toBeInTheDocument();
    expect(within(card).queryByText('授权过期，请重新登录')).not.toBeInTheDocument();
  }
});

it.each(['cloud.session.maintenanceDeferred', '网盘请求失败：cloud.session.maintenanceDeferred'])('localizes a temporary provider maintenance failure (%s)', async message => {
  const client = createClient();
  client.getCloudProviderCatalog.mockResolvedValue([cloudCatalog[1]]);
  client.getCloudAccount.mockRejectedValue({ code: 'provider_error', message });
  render(<CloudWorkspace client={client} />);
  expect(await screen.findByRole('alert')).toHaveTextContent('网盘暂时无法更新登录状态，请稍后重试。');
  expect(screen.getByRole('button', { name: '重试' })).toBeVisible();
  expect(screen.queryByLabelText('密码')).not.toBeInTheDocument();
});

it('renders the backend catalog order and labels', async () => {
  const client = createClient();
  client.getCloudProviderCatalog.mockResolvedValue([cloudCatalog[6], { ...cloudCatalog[0], displayName: '目录名称' }, cloudCatalog[2]]);
  render(<CloudWorkspace client={client as never} />);
  await screen.findByRole('region', { name: '夸克网盘账号授权' });
  expect(screen.getAllByRole('region').filter(region => region.getAttribute('aria-label')?.endsWith('账号授权')).map(region => region.getAttribute('aria-label'))).toEqual(['UC 网盘账号授权', '夸克网盘账号授权', '百度网盘账号授权']);
});

function createClient(accounts: Record<string, TestAccount | undefined> = {}) {
  const listeners = new Set<(event: any) => void>();
  let completedAccount: any = null;
  return {
    getCompletedCloudAuthorization: vi.fn(async () => completedAccount),
    subscribeCloudAuthorization: vi.fn(async (_provider: string, receive: (event: any) => void) => { listeners.add(receive); return () => { listeners.delete(receive); }; }),
    emitAuthorization: (account: any) => { completedAccount = account; listeners.forEach(receive => receive({ provider: account.provider, state: 'Ready', account })); },
    getCloudProviderCatalog: vi.fn().mockResolvedValue(cloudCatalog),
    beginCloudAuthorization: vi.fn().mockResolvedValue({ provider: 'quark', windowLabel: '登录', instructions: '完成登录' }),
    completeCloudAuthorization: vi.fn().mockImplementation((provider: 'quark' | 'pikpak') => Promise.resolve({ id: `${provider}-1`, displayName: `${provider}-alice`, provider, authMethod: 'web' as const })),
    loginCloudWithPassword: vi.fn().mockImplementation((provider: 'quark' | 'pikpak') => Promise.resolve({ id: `${provider}-1`, displayName: `${provider}-alice`, provider, authMethod: 'password' as const })),
    getCloudAccount: vi.fn().mockImplementation((provider: 'quark' | 'pikpak') => accounts[provider] ? Promise.resolve(accounts[provider]) : Promise.reject(Object.assign(new Error('未授权'), { code: 'unauthorized' }))),
    listCloudDirectory: vi.fn().mockImplementation((provider: string, parentId = '0') => Promise.resolve({ id: parentId, items: [{ id: `${provider}-file`, parentId, name: `${provider}-文件.txt`, kind: 'file', size: 10, modifiedAt: '刚刚' }] })),
    createCloudDirectory: vi.fn().mockResolvedValue(undefined), renameCloudFile: vi.fn().mockResolvedValue(undefined), moveCloudFiles: vi.fn().mockResolvedValue(undefined), copyCloudFiles: vi.fn().mockResolvedValue(undefined), trashCloudFiles: vi.fn().mockResolvedValue(undefined), deleteCloudFiles: vi.fn().mockResolvedValue(undefined), restoreCloudFiles: vi.fn().mockResolvedValue(undefined), createCloudShare: vi.fn().mockResolvedValue({ id: 'share-1', url: 'https://example.test/s/share-1', name: '分享文件', kind: 'file', sourceFileIds: [] }), listCloudShares: vi.fn().mockResolvedValue([]), cancelCloudShares: vi.fn().mockResolvedValue(undefined), downloadCloudFile: vi.fn().mockResolvedValue({}), downloadCloudDirectory: vi.fn().mockResolvedValue({ queuedCount: 0, tasks: [], failures: [] }), logoutCloud: vi.fn().mockResolvedValue(undefined), pickAndUploadFiles: vi.fn().mockResolvedValue({ uploaded: [], failures: [] }), pickAndUploadDirectory: vi.fn().mockResolvedValue({ uploaded: [], failures: [] }),
    getCloudAuthorizationCapabilities: vi.fn().mockImplementation((provider: string) => Promise.resolve({
      provider,
      web: true,
      password: provider === 'pikpak' || provider === 'baidu' || provider === 'tianyi',
      passwordLoginMode: provider === 'pikpak' || provider === 'baidu' || provider === 'tianyi'
        ? 'rustDirect'
        : provider === 'aliyun' || provider === 'xunlei' || provider === 'uc'
          ? 'officialWebView'
          : 'none'
    }))
  };
}

it('defaults to the cloud management page and lists every supported provider there', async () => {
  const client = createClient();
  render(<CloudWorkspace client={client} />);

  expect(await screen.findByRole('button', { name: '我的网盘' })).toHaveAttribute('aria-current', 'page');
  expect(screen.getByRole('region', { name: '网盘管理' })).toBeVisible();
  expect(screen.queryByText('添加网盘')).not.toBeInTheDocument();
  expect(screen.queryByText(/成功连接后会生成独立的文件管理标签/)).not.toBeInTheDocument();
  expect(screen.getByRole('region', { name: '夸克网盘账号授权' })).toBeVisible();
  expect(screen.getByRole('region', { name: 'PikPak 网盘账号授权' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '夸克网盘' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'PikPak 网盘' })).not.toBeInTheDocument();
  expect(screen.queryByRole('tree')).not.toBeInTheDocument();
  await waitFor(() => expect(client.getCloudAccount).toHaveBeenCalledTimes(7));
});

it('keeps one provider workspace submenu after logout and login as a different account', async () => {
  const user = userEvent.setup();
  const client = createClient({ quark: { id: 'old', displayName: 'old-alice', provider: 'quark', authMethod: 'web' } });
  client.getCloudProviderCatalog.mockResolvedValue([cloudCatalog[0]]);
  client.completeCloudAuthorization.mockResolvedValue({ id: 'new', displayName: 'new-bob', provider: 'quark', authMethod: 'web' });
  render(<CloudWorkspace client={client} />);
  await screen.findByText('· old-alice');
  await user.click(screen.getByRole('button', { name: '退出授权' }));
  await user.click(screen.getByRole('button', { name: '确认退出授权' }));
  await waitFor(() => expect(screen.queryByRole('button', { name: '夸克网盘' })).not.toBeInTheDocument());
  await user.click(screen.getByRole('button', { name: '使用网页授权' }));
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));
  expect(await screen.findByText('· new-bob')).toBeVisible();
  expect(screen.queryByText('· old-alice')).not.toBeInTheDocument();
  expect(screen.getAllByRole('button', { name: '夸克网盘' })).toHaveLength(1);
  await user.click(screen.getByRole('button', { name: '夸克网盘' }));
  expect(await screen.findByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
  expect(screen.queryByRole('tablist', { name: '夸克账号' })).not.toBeInTheDocument();
});

it('renders authorization cards for every newly supported cloud service', async () => {
  const client = createClient();
  render(<CloudWorkspace client={client as never} />);

  for (const label of ['百度网盘', '天翼云盘', '阿里云盘', '迅雷网盘']) {
    expect(await screen.findByRole('region', { name: `${label}账号授权` })).toBeVisible();
  }
  await waitFor(() => expect(client.getCloudAccount).toHaveBeenCalledTimes(7));
});

it('keeps the UC web authorization action available when the legacy capability endpoint is unavailable', async () => {
  const client = createClient();
  client.getCloudAuthorizationCapabilities.mockImplementation((provider: string) => provider === 'uc'
    ? Promise.reject(new Error('能力查询暂不可用'))
    : Promise.resolve({ provider, web: true, password: provider === 'pikpak' || provider === 'baidu' || provider === 'tianyi', passwordLoginMode: provider === 'pikpak' || provider === 'baidu' || provider === 'tianyi' ? 'rustDirect' : provider === 'aliyun' || provider === 'xunlei' ? 'officialWebView' : 'none' }));
  render(<CloudWorkspace client={client as never} />);

  const ucAuthorization = await screen.findByRole('region', { name: 'UC 网盘账号授权' });
  expect(within(ucAuthorization).getByRole('button', { name: '使用网页授权' })).toBeEnabled();
  expect(within(ucAuthorization).queryByLabelText('UC账号')).not.toBeInTheDocument();
  expect(client.getCloudAuthorizationCapabilities).not.toHaveBeenCalled();
});

it('opens a recovered UC account at root 0', async () => {
  const user = userEvent.setup();
  const client = createClient({
    uc: { id: 'uc-1', displayName: 'uc-alice', provider: 'uc' as never, authMethod: 'web' }
  });
  render(<CloudWorkspace client={client as never} />);

  await user.click(await screen.findByRole('button', { name: 'UC 网盘' }));
  expect(await screen.findByRole('tree', { name: 'UC 网盘文件' })).toBeVisible();
  expect(client.listCloudDirectory).toHaveBeenLastCalledWith('uc', '0');
});

it('ignores a recovered account when the provider does not match the request', async () => {
  const client = createClient();
  client.getCloudAccount.mockResolvedValue({ id: 'p-1', displayName: 'p-alice', provider: 'pikpak', authMethod: 'password' });
  render(<CloudWorkspace client={client as never} />);

  await waitFor(() => expect(client.getCloudAccount).toHaveBeenCalledTimes(7));
  expect(screen.getAllByRole('button', { name: 'PikPak 网盘' })).toHaveLength(1);
  expect(screen.queryByRole('button', { name: '夸克网盘' })).not.toBeInTheDocument();
});

it('shows the management shell and completed recoveries while a provider remains pending', async () => {
  let resolveQuark!: (account: TestAccount) => void;
  let resolvePikpak!: (account: TestAccount) => void;
  const quark = new Promise<TestAccount>((resolve) => { resolveQuark = resolve; });
  const pikpak = new Promise<TestAccount>((resolve) => { resolvePikpak = resolve; });
  const client = createClient();
  client.getCloudAccount.mockImplementation((provider: 'quark' | 'pikpak') => provider === 'quark' ? quark : pikpak);

  render(<CloudWorkspace client={client} />);

  expect(screen.getByRole('button', { name: '我的网盘' })).toBeVisible();
  expect(screen.getByRole('status', { name: '正在读取网盘账号状态' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '使用网页授权' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '登录PikPak账号' })).not.toBeInTheDocument();

  resolveQuark({ id: 'q-1', displayName: 'q-alice', provider: 'quark', authMethod: 'web' });
  expect(await screen.findByRole('button', { name: '夸克网盘' })).toBeVisible();
  expect(screen.queryByRole('button', { name: 'PikPak 网盘' })).not.toBeInTheDocument();
  resolvePikpak({ id: 'p-1', displayName: 'p-alice', provider: 'pikpak', authMethod: 'password' });
  await screen.findByRole('button', { name: 'PikPak 网盘' });
  expect(screen.getByRole('button', { name: 'PikPak 网盘' })).toBeVisible();
  expect(screen.queryByRole('status', { name: '正在读取网盘账号状态' })).not.toBeInTheDocument();
});

it('adds a provider submenu after priority password login and keeps account controls in management', async () => {
  const user = userEvent.setup();
  const client = createClient();
  render(<CloudWorkspace client={client} />);

  const pikpakAuthorization = await screen.findByRole('region', { name: 'PikPak 网盘账号授权' });
  await user.type(within(pikpakAuthorization).getByLabelText('账号'), 'alice');
  await user.type(within(pikpakAuthorization).getByLabelText('密码'), 'secret');
  await user.click(within(pikpakAuthorization).getByRole('button', { name: '登录' }));

  expect(await screen.findByRole('button', { name: 'PikPak 网盘' })).not.toHaveAttribute('aria-current');
  expect(screen.getByRole('button', { name: '我的网盘' })).toHaveAttribute('aria-current', 'page');
  expect(screen.getByRole('button', { name: '已经登录' })).toBeDisabled();
  expect(screen.getByRole('button', { name: '退出登录' })).toBeVisible();
  expect(screen.queryByRole('tree')).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: 'PikPak 网盘' }));
  expect(await screen.findByRole('tree', { name: 'PikPak 网盘文件' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '已经登录' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '退出登录' })).not.toBeInTheDocument();
});

it('adds a Quark file submenu after web authorization without moving authorization actions into it', async () => {
  const user = userEvent.setup();
  const client = createClient();
  render(<CloudWorkspace client={client} />);

  await user.click((await screen.findAllByRole('button', { name: '使用网页授权' }))[0]);
  await user.click(screen.getByRole('button', { name: '我已完成授权' }));

  expect(await screen.findByRole('button', { name: '夸克网盘' })).not.toHaveAttribute('aria-current');
  expect(screen.getByRole('button', { name: '已经完成授权' })).toBeDisabled();
  expect(screen.getByRole('button', { name: '退出授权' })).toBeVisible();

  await user.click(screen.getByRole('button', { name: '夸克网盘' }));
  expect(await screen.findByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
  expect(screen.queryByText('已经完成授权')).not.toBeInTheDocument();
  expect(screen.queryByText('退出授权')).not.toBeInTheDocument();
});

it('automatically adds the Aliyun workspace submenu after web authorization completes', async () => {
  const user = userEvent.setup();
  const client = createClient();
  render(<CloudWorkspace client={client as never} />);

  const aliyunAuthorization = await screen.findByRole('region', { name: '阿里云盘账号授权' });
  await user.click(within(aliyunAuthorization).getByRole('button', { name: '使用网页授权' }));
  await act(async () => { client.emitAuthorization({ id: 'ali', displayName: 'alice', provider: 'aliyun', authMethod: 'web' }); });

  expect(await screen.findByRole('button', { name: '阿里云盘' }, { timeout: 1_500 })).not.toHaveAttribute('aria-current');
  expect(client.completeCloudAuthorization).not.toHaveBeenCalled();
});

it('recovers accounts into submenus while still opening on cloud management', async () => {
  const client = createClient({ quark: { id: 'q-1', displayName: 'q-alice', provider: 'quark', authMethod: 'web' } });
  render(<CloudWorkspace client={client} />);

  expect(await screen.findByRole('button', { name: '夸克网盘' })).not.toHaveAttribute('aria-current');
  expect(screen.getByRole('button', { name: '我的网盘' })).toHaveAttribute('aria-current', 'page');
  expect(screen.queryByRole('tree')).not.toBeInTheDocument();
  expect(client.listCloudDirectory).not.toHaveBeenCalled();
  await waitFor(() => expect(client.getCloudAccount).toHaveBeenCalledTimes(7));
});

it('shows only authorized provider submenus and switches their real directory views', async () => {
  const user = userEvent.setup();
  const client = createClient({ quark: { id: 'q-1', displayName: 'q-alice', provider: 'quark', authMethod: 'web' }, pikpak: { id: 'p-1', displayName: 'p-alice', provider: 'pikpak', authMethod: 'password' } });
  render(<CloudWorkspace client={client} />);

  await screen.findByRole('button', { name: '夸克网盘' });
  expect(screen.getByRole('button', { name: 'PikPak 网盘' })).toBeVisible();
  await user.click(screen.getByRole('button', { name: '夸克网盘' }));
  expect(await screen.findByText('quark-文件.txt')).toBeVisible();
  await user.click(screen.getByRole('button', { name: 'PikPak 网盘' }));
  expect(await screen.findByText('pikpak-文件.txt')).toBeVisible();
});

it('logs out from management, removes the provider submenu, and stays on management', async () => {
  const user = userEvent.setup();
  const client = createClient({ quark: { id: 'q-1', displayName: 'q-alice', provider: 'quark', authMethod: 'web' } });
  render(<CloudWorkspace client={client} />);

  await screen.findByRole('button', { name: '夸克网盘' });
  await user.click(screen.getByRole('button', { name: '退出授权' }));
  await user.click(screen.getByRole('button', { name: '确认退出授权' }));

  await waitFor(() => expect(screen.queryByRole('button', { name: '夸克网盘' })).not.toBeInTheDocument());
  expect(screen.getByRole('button', { name: '我的网盘' })).toHaveAttribute('aria-current', 'page');
  expect(screen.getByRole('region', { name: '夸克网盘账号授权' })).toBeVisible();
  expect(screen.queryByRole('tree')).not.toBeInTheDocument();
});

it('returns to management if the removed provider is selected while logout is pending', async () => {
  const user = userEvent.setup();
  const client = createClient({ quark: { id: 'q-1', displayName: 'q-alice', provider: 'quark', authMethod: 'web' } });
  let finishLogout!: () => void;
  client.logoutCloud.mockReturnValue(new Promise<void>(resolve => { finishLogout = resolve; }));
  render(<CloudWorkspace client={client} />);
  const submenu = await screen.findByRole('button', { name: '夸克网盘' });
  await user.click(screen.getByRole('button', { name: '退出授权' }));
  await user.click(screen.getByRole('button', { name: '确认退出授权' }));
  await user.click(submenu);
  expect(await screen.findByRole('tree', { name: '夸克网盘文件' })).toBeVisible();
  await act(async () => { finishLogout(); });
  expect(screen.queryByRole('button', { name: '夸克网盘' })).not.toBeInTheDocument();
  expect(screen.getByRole('region', { name: '网盘管理' })).toBeVisible();
  expect(screen.getByRole('button', { name: '我的网盘' })).toHaveAttribute('aria-current', 'page');
});

it('follows the pointer with a provider card preview and saves its calculated insertion order on release', async () => {
  class TestPointerEvent extends MouseEvent {
    pointerId: number;
    constructor(type: string, init: MouseEventInit & { pointerId?: number } = {}) {
      super(type, init);
      this.pointerId = init.pointerId ?? 0;
    }
  }
  const originalPointerEvent = window.PointerEvent;
  Object.defineProperty(window, 'PointerEvent', { configurable: true, value: TestPointerEvent });
  const originalElementFromPoint = document.elementFromPoint;
  const storageKey = 'versadown.cloud-provider-card-order';
  window.localStorage.removeItem(storageKey);
  const client = createClient();
  client.getCloudProviderCatalog.mockResolvedValue([cloudCatalog[0], cloudCatalog[1], cloudCatalog[2]]);
  const view = render(<CloudWorkspace client={client} />);

  const quarkCard = await screen.findByRole('region', { name: '夸克网盘账号授权' });
  const pikpakCard = screen.getByRole('region', { name: 'PikPak 网盘账号授权' });
  const baiduCard = screen.getByRole('region', { name: '百度网盘账号授权' });
  const grid = screen.getByRole('region', { name: '网盘管理' }).querySelector('.cloud-provider-auths')!;
  const quarkHeader = within(quarkCard).getByRole('heading', { name: '夸克网盘' }).closest('header')!;
  const infoButton = within(quarkCard).getByRole('button', { name: '查看夸克网盘信息' });
  const rect = (left: number, top: number) => ({ top, bottom: top + 120, height: 120, left, right: left + 240, width: 240, x: left, y: top, toJSON: () => ({}) });
  vi.spyOn(quarkCard, 'getBoundingClientRect').mockReturnValue(rect(0, 0));
  vi.spyOn(baiduCard, 'getBoundingClientRect').mockReturnValue(rect(0, 160));
  vi.spyOn(grid, 'getBoundingClientRect').mockReturnValue({ top: 0, bottom: 320, height: 320, left: 0, right: 500, width: 500, x: 0, y: 0, toJSON: () => ({}) });
  Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: vi.fn(() => baiduCard) });

  fireEvent.pointerDown(quarkHeader, { pointerId: 7, button: 0, clientX: 20, clientY: 20 });
  fireEvent.pointerMove(quarkHeader, { pointerId: 7, clientX: 210, clientY: 270 });

  const preview = document.querySelector<HTMLElement>('.cloud-provider-drag-preview');
  expect(preview).toHaveStyle({ left: '190px', top: '250px' });
  expect(screen.getAllByRole('region').filter(region => region.getAttribute('aria-label')?.endsWith('账号授权')).map(region => region.getAttribute('aria-label'))).toEqual(['PikPak 网盘账号授权', '百度网盘账号授权', '夸克网盘账号授权']);
  expect(baiduCard).toHaveClass('cloud-provider-drop-target');
  expect(pikpakCard).not.toHaveClass('cloud-provider-drop-target');

  fireEvent.pointerUp(quarkHeader, { pointerId: 7, clientX: 210, clientY: 270 });
  expect(document.querySelector('.cloud-provider-drag-preview')).not.toBeInTheDocument();
  expect(JSON.parse(window.localStorage.getItem(storageKey) ?? '[]')).toEqual(['pikpak', 'baidu', 'quark']);
  expect(pikpakCard).toBeVisible();
  fireEvent.click(infoButton);
  expect(within(quarkCard).getByText('官方网站')).toBeVisible();
  view.unmount();
  render(<CloudWorkspace client={client} />);
  await screen.findByRole('region', { name: '夸克网盘账号授权' });
  expect(screen.getAllByRole('region').filter(region => region.getAttribute('aria-label')?.endsWith('账号授权')).map(region => region.getAttribute('aria-label'))).toEqual(['PikPak 网盘账号授权', '百度网盘账号授权', '夸克网盘账号授权']);
  window.localStorage.removeItem(storageKey);
  if (originalElementFromPoint) Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: originalElementFromPoint });
  else delete (document as Document & { elementFromPoint?: typeof document.elementFromPoint }).elementFromPoint;
  if (originalPointerEvent) Object.defineProperty(window, 'PointerEvent', { configurable: true, value: originalPointerEvent });
  else delete (window as Window & { PointerEvent?: typeof window.PointerEvent }).PointerEvent;
});
