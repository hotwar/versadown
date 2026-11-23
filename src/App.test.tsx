import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { App } from './App';
import './styles.css';

const cloudApiMocks = vi.hoisted(() => ({
  getCloudProviderCatalog: vi.fn().mockImplementation(async () => (await import('./test/cloudCatalog')).cloudCatalog),
  getCloudAuthorizationCapabilities: vi.fn().mockImplementation((provider: string) => Promise.resolve({ provider, web: true, password: false })),
  beginCloudAuthorization: vi.fn(),
  completeCloudAuthorization: vi.fn(),
  subscribeCloudAuthorization: vi.fn().mockResolvedValue(() => {}),
  getCompletedCloudAuthorization: vi.fn().mockResolvedValue(null),
  reauthorizeFailedCloudTask: vi.fn(),
  loginCloudWithPassword: vi.fn(),
  loginMega: vi.fn(),
  getSavedCloudCredentials: vi.fn().mockResolvedValue(null),
  getCloudAccount: vi.fn().mockRejectedValue({ code: 'not_authorized', message: '未授权' }),
  getCloudAccountCapacity: vi.fn().mockImplementation((provider: string) => Promise.resolve({
    provider,
    accountId: `${provider}-account`,
    capacity: { kind: 'limited', totalBytes: 100, usedBytes: 25 },
    updatedAt: 1
  })),
  subscribeCloudCapacityInvalidated: vi.fn().mockResolvedValue(() => {}),
  listCloudDirectory: vi.fn(),
  createCloudDirectory: vi.fn(),
  renameCloudFile: vi.fn(),
  moveCloudFiles: vi.fn(),
  copyCloudFiles: vi.fn(),
  trashCloudFiles: vi.fn(),
  deleteCloudFiles: vi.fn(),
  restoreCloudFiles: vi.fn(),
  createCloudShare: vi.fn(),
  listCloudShares: vi.fn().mockResolvedValue([]),
  cancelCloudShares: vi.fn(),
  downloadCloudFile: vi.fn(),
  downloadCloudDirectory: vi.fn(),
  logoutCloud: vi.fn(),
  pickAndUploadFiles: vi.fn(),
  pickAndUploadDirectory: vi.fn()
}));

vi.mock('./lib/cloudApi', () => cloudApiMocks);

it('places authorized cloud submenus below My Cloud and opens their files from any main view', async () => {
  const user = userEvent.setup();
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  cloudApiMocks.getCloudAccount.mockClear();
  cloudApiMocks.getCloudAccount.mockImplementation((provider: string) => ['quark', 'pikpak'].includes(provider)
    ? Promise.resolve({ provider, id: `${provider}-account`, displayName: provider, authMethod: 'web' })
    : Promise.reject({ code: 'not_authorized' }));
  cloudApiMocks.listCloudDirectory.mockImplementation((provider: string) => Promise.resolve({ id: '0', items: [{ id: `${provider}-file`, parentId: '0', name: `${provider}-sample.txt`, kind: 'file', size: 10 }] }));
  try {
    render(<App />);
    const navigation = screen.getByRole('navigation', { name: '主导航' });
    const quark = await within(navigation).findByRole('button', { name: '夸克网盘' });
    const pikpak = within(navigation).getByRole('button', { name: 'PikPak 网盘' });
    const labels = within(navigation).getAllByRole('button').map(button => button.textContent);
    expect(labels.indexOf('夸克网盘')).toBeGreaterThan(labels.indexOf('我的网盘'));
    expect(labels.indexOf('PikPak 网盘')).toBeLessThan(labels.indexOf('设置'));
    expect(quark.querySelector('svg')).toBeNull();
    expect(quark).toHaveStyle({ fontSize: '14px', paddingLeft: '44px' });
    expect(within(navigation).getByRole('button', { name: '我的网盘' })).toHaveStyle({ fontSize: '15px', gridTemplateColumns: '28px 1fr auto', gap: '6px', paddingLeft: '10px' });
    expect(quark).not.toHaveAttribute('aria-current');

    await user.click(quark);
    expect(await screen.findByText('quark-sample.txt')).toBeVisible();
    expect(quark).toHaveAttribute('aria-current', 'page');
    expect(screen.queryByRole('tablist', { name: '我的网盘标签' })).not.toBeInTheDocument();
    await user.click(pikpak);
    expect(await screen.findByText('pikpak-sample.txt')).toBeVisible();
    expect(pikpak).toHaveAttribute('aria-current', 'page');
    expect(quark).not.toHaveAttribute('aria-current');

    await user.click(within(navigation).getByRole('button', { name: /任务/ }));
    expect(pikpak).toBeVisible();
    expect(pikpak).not.toHaveAttribute('aria-current');
    await user.click(within(navigation).getByRole('button', { name: '我的网盘' }));
    expect(screen.getByRole('region', { name: '网盘管理' })).toBeVisible();
    expect(within(navigation).getByRole('button', { name: '我的网盘' })).toHaveAttribute('aria-current', 'page');
    expect(cloudApiMocks.getCloudAccount).toHaveBeenCalledTimes(8);
  } finally {
    stylesheet.remove();
    cloudApiMocks.getCloudAccount.mockRejectedValue({ code: 'not_authorized', message: '未授权' });
  }
});

it('adds a sidebar cloud submenu after authorization and removes it after logout', async () => {
  const user = userEvent.setup();
  cloudApiMocks.getCloudAccount.mockRejectedValue({ code: 'not_authorized' });
  cloudApiMocks.beginCloudAuthorization.mockResolvedValue({ provider: 'quark', windowLabel: '登录', instructions: '完成登录' });
  cloudApiMocks.completeCloudAuthorization.mockResolvedValue({ provider: 'quark', id: 'new-account', displayName: 'new-user', authMethod: 'web' });
  cloudApiMocks.logoutCloud.mockResolvedValue(undefined);
  render(<App />);
  const navigation = screen.getByRole('navigation', { name: '主导航' });
  await user.click(within(navigation).getByRole('button', { name: '我的网盘' }));
  expect(within(navigation).queryByRole('button', { name: '夸克网盘' })).not.toBeInTheDocument();
  const card = await screen.findByRole('region', { name: '夸克网盘账号授权' });
  await user.click(within(card).getByRole('button', { name: '使用网页授权' }));
  await user.click(await screen.findByRole('button', { name: '我已完成授权' }));
  expect(await within(navigation).findByRole('button', { name: '夸克网盘' })).toBeVisible();
  expect(within(navigation).getAllByRole('button', { name: '夸克网盘' })).toHaveLength(1);
  await user.click(screen.getByRole('button', { name: '退出授权' }));
  await user.click(screen.getByRole('button', { name: '确认退出授权' }));
  await waitFor(() => expect(within(navigation).queryByRole('button', { name: '夸克网盘' })).not.toBeInTheDocument());
  expect(screen.getByRole('region', { name: '网盘管理' })).toBeVisible();
});

it('renders the compact download-manager layout without browser navigation controls', async () => {
  render(<App />);
  expect(screen.getByRole('heading', { name: '万能下载' })).toBeVisible();
  expect(screen.getByPlaceholderText('此处可搜索本地任务，粘贴下载链接，拖入 BT 种子文件')).toBeVisible();
  expect(screen.queryByRole('button', { name: '新建下载' })).not.toBeInTheDocument();
  expect(screen.getByRole('tab', { name: /全部/ })).toBeVisible();
  expect(screen.getByRole('tab', { name: /下载中/ })).toBeVisible();
  expect(screen.getByRole('tab', { name: /已完成/ })).toBeVisible();
  expect(screen.getByRole('tab', { name: /失败/ })).toBeVisible();
  expect(screen.queryByRole('tab', { name: /回收站/ })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '后退' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '前进' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '刷新' })).not.toBeInTheDocument();
  await waitFor(() => expect(document.querySelector('.cloud-workspace-recovering')).not.toBeInTheDocument());
});

it('prevents selecting application chrome while keeping editable text selectable', async () => {
  render(<App />);

  const appShell = document.querySelector('.app-shell');
  const searchIcon = document.querySelector('.global-search > span');
  expect(appShell).toHaveStyle({ userSelect: 'none' });
  expect(appShell).toContainElement(within(screen.getByRole('navigation', { name: '主导航' })).getByText('任务'));
  expect(appShell).toContainElement(searchIcon);
  expect(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' })).toHaveStyle({ userSelect: 'text' });
  await waitFor(() => expect(document.querySelector('.cloud-workspace-recovering')).not.toBeInTheDocument());
});

it('leaves quick input to the independent native launcher window', async () => {
  render(<App />);
  expect(screen.queryByRole('button', { name: '打开下载输入' })).not.toBeInTheDocument();
  expect(screen.getByRole('region', { name: '下载队列' })).toBeVisible();
  await waitFor(() => expect(document.querySelector('.cloud-workspace-recovering')).not.toBeInTheDocument());
});

it('labels the primary download workspace as tasks', async () => {
  render(<App />);

  expect(within(screen.getByRole('navigation', { name: '主导航' })).getByRole('button', { name: /任务/ })).toBeVisible();
  await waitFor(() => expect(document.querySelector('.cloud-workspace-recovering')).not.toBeInTheDocument());
});

it('places the sidebar toggle after the app title and keeps it out of the download tabs', async () => {
  const user = userEvent.setup();
  render(<App />);
  const navigation = screen.getByRole('navigation', { name: '主导航' });
  const taskTabs = screen.getByRole('tablist', { name: '下载任务' });
  const appHeader = screen.getByRole('heading', { name: '万能下载' }).closest('.app-header');

  expect(within(taskTabs).queryByRole('tab', { name: '新建下载' })).not.toBeInTheDocument();
  expect(appHeader).not.toBeNull();
  expect(within(taskTabs).queryByRole('button', { name: '收起侧边栏' })).not.toBeInTheDocument();
  await user.click(within(appHeader!).getByRole('button', { name: '收起侧边栏' }));
  expect(within(appHeader!).getByRole('button', { name: '展开侧边栏' })).toHaveAttribute('aria-expanded', 'false');
  expect(navigation).toHaveAttribute('aria-hidden', 'true');

  await user.click(within(appHeader!).getByRole('button', { name: '展开侧边栏' }));
  expect(within(appHeader!).getByRole('button', { name: '收起侧边栏' })).toHaveAttribute('aria-expanded', 'true');
  expect(navigation).toHaveAttribute('aria-hidden', 'false');
});

it('keeps the total transfer speed at the right edge of the download tab row', async () => {
  render(<App />);

  expect(screen.getByLabelText('当前总下载速度')).toHaveTextContent('0 B/s');
  expect(screen.queryByText('当前速度')).not.toBeInTheDocument();
  expect(screen.queryByText(/本地下载引擎/)).not.toBeInTheDocument();
  await waitFor(() => expect(document.querySelector('.cloud-workspace-recovering')).not.toBeInTheDocument());
});

it('keeps the custom window controls pinned to the header trailing edge', async () => {
  render(<App />);

  expect(screen.getByLabelText('窗口控制')).toHaveStyle({
    marginLeft: 'auto',
    flex: '0 0 auto'
  });
  await waitFor(() => expect(document.querySelector('.cloud-workspace-recovering')).not.toBeInTheDocument());
});

it('uses a one-line download link to open the unified new-task dialog', async () => {
  const user = userEvent.setup();
  render(<App />);

  expect(screen.queryByRole('dialog', { name: '新建下载' })).not.toBeInTheDocument();
  await user.type(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), 'https://example.com/archive.zip{Enter}');
  const dialog = screen.getByRole('dialog', { name: '新建下载' });
  expect(dialog).toHaveClass('batch-download-dialog', 'modal');
  expect(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' })).toBeDisabled();
});

it('opens the cloud management workspace and restores each provider once', async () => {
  const user = userEvent.setup();
  cloudApiMocks.getCloudAccount.mockClear();
  render(<App />);

  await user.click(screen.getByRole('button', { name: /我的网盘/ }));

  expect(await screen.findByRole('region', { name: '网盘管理' })).toBeVisible();
  expect(screen.getByRole('button', { name: '我的网盘' })).toHaveAttribute('aria-current', 'page');
  expect(cloudApiMocks.getCloudAccount).toHaveBeenCalledTimes(8);
  expect(cloudApiMocks.getCloudAccount).toHaveBeenCalledWith('quark');
  expect(cloudApiMocks.getCloudAccount).toHaveBeenCalledWith('pikpak');
  expect(cloudApiMocks.getCloudAccount).toHaveBeenCalledWith('aliyun');
  expect(cloudApiMocks.getCloudAccount).toHaveBeenCalledWith('uc');
});

it('preloads cloud authorization before the user opens My Cloud', async () => {
  const user = userEvent.setup();
  cloudApiMocks.getCloudAccount.mockClear();
  cloudApiMocks.getCloudAccount.mockImplementation((provider: string) => provider === 'quark'
    ? Promise.resolve({ provider: 'quark', id: 'quark-account', displayName: '我的夸克', authMethod: 'web' })
    : Promise.reject({ code: 'not_authorized', message: '未授权' }));

  render(<App />);

  await waitFor(() => expect(cloudApiMocks.getCloudAccount).toHaveBeenCalledTimes(8));
  expect(screen.queryByRole('region', { name: '网盘管理' })).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: /我的网盘/ }));

  expect(screen.getByRole('region', { name: '网盘管理' })).toBeVisible();
  expect(within(screen.getByRole('navigation', { name: '主导航' })).getByRole('button', { name: '夸克网盘' })).toBeVisible();
  expect(screen.getByRole('button', { name: '已经完成授权' })).toBeDisabled();
});

it('adds a native cloud download result to the visible download queue', async () => {
  const user = userEvent.setup();
  cloudApiMocks.getCloudAccount.mockImplementation((provider: string) => provider === 'quark'
    ? Promise.resolve({ provider: 'quark', id: 'quark-account', displayName: '我的夸克', authMethod: 'web' })
    : Promise.reject({ code: 'not_authorized', message: '未授权' }));
  cloudApiMocks.listCloudDirectory.mockResolvedValue({ id: '0', items: [{ id: 'cloud-file', parentId: '0', name: '网盘电影.mkv', kind: 'file', size: 1024 }] });
  cloudApiMocks.downloadCloudFile.mockResolvedValue({ id: 'cloud-task', source: 'cloud://quark/cloud-file', name: '网盘电影.mkv', kind: 'cloud', status: 'downloading', progress: 0 });
  render(<App />);

  await user.click(screen.getByRole('button', { name: /我的网盘/ }));
  await user.click(await screen.findByRole('button', { name: '夸克网盘' }));
  await user.click(await screen.findByRole('treeitem', { name: /网盘电影\.mkv/ }));
  await user.click(screen.getByRole('button', { name: '下载', exact: true }));

  expect(await screen.findByText('已将「网盘电影.mkv」加入下载队列')).toBeVisible();
  await user.click(within(screen.getByRole('navigation', { name: '主导航' })).getByRole('button', { name: /任务/ }));
  expect(within(screen.getByRole('region', { name: '下载队列' })).getByTitle('网盘电影.mkv')).toBeVisible();
});

it('shows task details only after the task context-menu command', async () => {
  const user = userEvent.setup();
  render(<App />);
  const search = screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' });

  await user.type(search, 'https://example.com/archive.zip{Enter}');
  const newTaskDialog = screen.getByRole('dialog', { name: '新建下载' });
  await screen.findByText('archive.zip');
  await user.click(within(newTaskDialog).getByRole('button', { name: '新建任务' }));
  expect(await screen.findByTitle('archive.zip')).toBeVisible();
  expect(screen.queryByRole('dialog', { name: '任务详情' })).not.toBeInTheDocument();

  await user.click(screen.getByTitle('archive.zip'));
  expect(screen.queryByRole('dialog', { name: '任务详情' })).not.toBeInTheDocument();

  fireEvent.contextMenu(screen.getByTitle('archive.zip'), { clientX: 320, clientY: 220 });
  await user.click(screen.getByRole('menuitem', { name: '查看任务详情' }));
  expect(screen.getByRole('dialog', { name: '任务详情' })).toBeVisible();
  expect(search).toBeDisabled();
  await user.click(screen.getByRole('button', { name: '关闭任务详情' }));
  expect(search).toBeEnabled();
});
