import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { readFileSync } from 'node:fs';
import { beforeEach, expect, it, vi } from 'vitest';
import type { QueueTask, QueueTaskInput } from './lib/types';
import { getThemeMode, setThemeMode } from './lib/theme';
import { cloudCatalog } from './test/cloudCatalog';
import { scanDownloadInputs } from './lib/api';
import { t } from './i18n';

const state = vi.hoisted(() => ({
  list: vi.fn(), parse: vi.fn(), onTask: undefined as undefined | ((task: QueueTaskInput) => void),
  onFocus: undefined as undefined | ((event: { payload: boolean }) => void),
  listeners: new Map<string, (event: { payload: unknown }) => void>(), listen: vi.fn(),
  confirm: vi.fn(), confirmCloud: vi.fn(), discardPrepared: vi.fn(), resolveDuplicate: vi.fn(), openDirectory: vi.fn(), selectFiles: vi.fn(), cancelFile: vi.fn(), getEngineState: vi.fn(), retryEngine: vi.fn(), taskAction: vi.fn(),
  moveToRecycle: vi.fn(), purgeTasks: vi.fn(), deleteTasks: vi.fn(), setTaskPrivacy: vi.fn(), clipboardWrite: vi.fn(), clipboardRead: vi.fn(),
  updateAiSettings: vi.fn(), listAiModels: vi.fn(), startDragging: vi.fn().mockResolvedValue(undefined),
  reauthorizeFailedCloudTask: vi.fn(), completeCloudAuthorization: vi.fn(), getCompletedCloudAuthorization: vi.fn(),
  getSettings: vi.fn(), configurePrivacy: vi.fn(), privacyVisibility: vi.fn(), windowSnapshot: vi.fn(),
  updateCheck: vi.fn(), openExternalUrl: vi.fn(), cloudInvoke: vi.fn(),
  dragDropHandler: undefined as undefined | ((event: { payload: unknown }) => void),
  onDragDropEvent: vi.fn(), scaleFactor: vi.fn(), takeExternalLinks: vi.fn()
}));

vi.mock('./lib/api', () => ({
  isTauriRuntime: () => true, listPersistedTasks: state.list, parseDownload: state.parse,
  syncMainWindowIcon: vi.fn().mockResolvedValue(undefined), takeExternalLinks: state.takeExternalLinks,
  scanDownloadInputs: vi.fn().mockResolvedValue([]),
  confirmDownload: state.confirm, confirmCloudShareDownload: state.confirmCloud, discardPreparedDownload: state.discardPrepared, runTaskAction: state.taskAction, selectDownloadFiles: state.selectFiles, cancelDownloadFile: state.cancelFile,
  getDownloadEngineState: state.getEngineState, retryDownloadEngine: state.retryEngine,
  getPrivacyTaskVisibility: state.privacyVisibility,
  checkForAppUpdate: state.updateCheck, openExternalUrl: state.openExternalUrl,
  defaultSettings: { downloadDirectory: 'C:\\Downloads', launcherVisibility: 'always', proxy: { enabled: false, protocol: 'http', host: '', port: 7890, username: '', password: '' }, magnetMetadata: { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' }, aria2: { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 5, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false } },
  getSettings: state.getSettings, configurePrivacyDownload: state.configurePrivacy,
  updateSettings: vi.fn(), updateAiSettings: state.updateAiSettings, listAiModels: state.listAiModels, openTaskDirectory: state.openDirectory,
  moveTaskToRecycle: state.moveToRecycle, purgeRecycledTasks: state.purgeTasks, deleteTasks: state.deleteTasks, setTaskPrivacy: state.setTaskPrivacy, resolveDuplicateDownload: state.resolveDuplicate
}));
vi.mock('./lib/downloadEvents', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./lib/downloadEvents')>();
  return { ...actual, subscribeToDownloadEvents: vi.fn().mockImplementation(({ onTask }) => {
    state.onTask = onTask;
    return Promise.resolve(() => undefined);
  }) };
});
vi.mock('./lib/cloudApi', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./lib/cloudApi')>();
  return {
    ...actual,
    reauthorizeFailedCloudTask: state.reauthorizeFailedCloudTask,
    completeCloudAuthorization: state.completeCloudAuthorization,
    getCompletedCloudAuthorization: state.getCompletedCloudAuthorization,
  };
});
vi.mock('@tauri-apps/api/event', () => ({ listen: state.listen }));
vi.mock('@tauri-apps/api/core', async (importOriginal) => ({
  ...await importOriginal<typeof import('@tauri-apps/api/core')>(),
  invoke: (command: string, args: unknown) => command === 'main_window_interaction_state'
    ? state.windowSnapshot() : state.cloudInvoke(command, args),
}));
vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({ writeText: state.clipboardWrite, readText: state.clipboardRead }));
vi.mock('@tauri-apps/api/window', async (importOriginal) => ({
  ...await importOriginal<typeof import('@tauri-apps/api/window')>(),
  getCurrentWindow: () => ({
  onFocusChanged: vi.fn().mockImplementation((handler) => { state.onFocus = handler; return Promise.resolve(() => undefined); }),
  onResized: vi.fn().mockResolvedValue(() => undefined),
  onScaleChanged: vi.fn().mockResolvedValue(() => undefined),
  onDragDropEvent: state.onDragDropEvent,
  scaleFactor: state.scaleFactor,
  setMinSize: vi.fn().mockResolvedValue(undefined),
  setSize: vi.fn().mockResolvedValue(undefined),
  startDragging: state.startDragging
  }),
}));
import { App } from './App';

const oldTask: QueueTask = { id: 'task-1', name: 'archive.iso', kind: 'file', status: 'downloading', progress: 10, total_length: 100, completed_length: 10 };
const liveTask: QueueTask = { ...oldTask, progress: 40, completed_length: 40 };

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

beforeEach(() => {
  window.localStorage.removeItem('remove-task-delete-files');
  state.cloudInvoke.mockReset().mockImplementation((command: string) => Promise.reject(new Error(`Unexpected command: ${command}`)));
  state.windowSnapshot.mockReset();
  state.windowSnapshot.mockResolvedValue({ active: false, nativeDrag: false });
  state.list.mockReset();
  state.list.mockResolvedValue([]);
  state.parse.mockReset();
  state.confirm.mockReset();
  state.confirmCloud.mockReset();
  state.discardPrepared.mockReset().mockResolvedValue(undefined);
  state.resolveDuplicate.mockReset();
  state.openDirectory.mockReset();
  state.selectFiles.mockReset();
  state.cancelFile.mockReset();
  state.getEngineState.mockReset();
  state.getEngineState.mockResolvedValue({ available: true, retrying: false });
  state.retryEngine.mockReset();
  state.taskAction.mockReset();
  state.moveToRecycle.mockReset();
  state.purgeTasks.mockReset();
  state.deleteTasks.mockReset();
  state.setTaskPrivacy.mockReset();
  state.clipboardWrite.mockReset();
  state.clipboardRead.mockReset().mockResolvedValue('');
  state.updateAiSettings.mockReset();
  state.listAiModels.mockReset();
  state.startDragging.mockReset();
  state.startDragging.mockResolvedValue(undefined);
  state.reauthorizeFailedCloudTask.mockReset();
  state.getCompletedCloudAuthorization.mockReset().mockResolvedValue(null);
  state.completeCloudAuthorization.mockReset();
  state.getSettings.mockReset();
  state.getSettings.mockResolvedValue({ downloadDirectory: 'C:\\Downloads', launcherVisibility: 'always', proxy: { enabled: false, protocol: 'http', host: '', port: 7890, username: '', password: '' }, magnetMetadata: { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' }, aria2: { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 5, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false } });
  state.configurePrivacy.mockReset();
  state.privacyVisibility.mockReset();
  state.privacyVisibility.mockResolvedValue({ enabled: false, unlocked: false });
  state.updateCheck.mockReset();
  state.updateCheck.mockResolvedValue(null);
  state.openExternalUrl.mockReset();
  state.openExternalUrl.mockResolvedValue(undefined);
  state.dragDropHandler = undefined;
  state.scaleFactor.mockReset().mockResolvedValue(1);
  state.takeExternalLinks.mockReset().mockResolvedValue([]);
  state.onDragDropEvent.mockReset().mockImplementation((handler) => {
    state.dragDropHandler = handler;
    return Promise.resolve(() => undefined);
  });
  state.onTask = undefined;
  state.onFocus = undefined;
  state.listeners.clear();
  state.listen.mockReset();
  state.listen.mockImplementation((name, handler) => {
    state.listeners.set(name, handler);
    return Promise.resolve(() => undefined);
  });
});

it('shows privacy task controls immediately after enabling an existing privacy password', async () => {
  const settings = {
    downloadDirectory: 'C:\\Downloads', launcherVisibility: 'always' as const,
    proxy: { enabled: false, protocol: 'http' as const, host: '', port: 7890, username: '', password: '' },
    ai: { enabled: false, baseUrl: '', model: '', apiKeyConfigured: false },
    magnetMetadata: { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' },
    aria2: { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 5, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false },
    deleteTransferredShareFilesAfterDownload: true,
    privacyDownloadEnabled: false,
    privacyPasswordConfigured: true,
  };
  state.getSettings.mockResolvedValue(settings);
  state.configurePrivacy.mockResolvedValue({ ...settings, privacyDownloadEnabled: true });
  state.privacyVisibility
    .mockResolvedValueOnce({ enabled: false, unlocked: false })
    .mockResolvedValue({ enabled: true, unlocked: false });
  const user = userEvent.setup();
  render(<App />);

  const navigation = screen.getByRole('navigation', { name: '主导航' });
  await user.click(within(navigation).getByRole('button', { name: '设置' }));
  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.click(screen.getByRole('checkbox', { name: '开启隐私下载' }));

  await waitFor(() => expect(state.configurePrivacy).toHaveBeenCalledWith(true, undefined, undefined));
  await user.click(within(navigation).getByRole('button', { name: /^任务/ }));
  expect(await screen.findByRole('button', { name: '解锁隐私任务' })).toBeVisible();

  fireEvent.paste(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), {
    clipboardData: { getData: () => 'https://example.test/private.zip' },
  });
  expect(within(await screen.findByRole('dialog', { name: '新建下载' })).getByRole('button', { name: '新建隐私任务' })).toBeVisible();
});

it('opens application information from About and returns to the task list', async () => {
  state.list.mockResolvedValueOnce([oldTask]);
  await act(async () => { render(<App />); });
  const navigation = screen.getByRole('navigation', { name: '主导航' });
  const about = within(navigation).getByRole('button', { name: '关于' });
  await userEvent.click(about);

  expect(about).toHaveClass('nav-active');
  expect(about).toHaveAttribute('aria-current', 'page');
  const page = screen.getByRole('region', { name: '关于' });
  expect(within(page).getByRole('heading', { name: '万能下载 VersaDown' })).toBeVisible();
  const configuration = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'));
  expect(within(page).getByText(`版本 ${configuration.version}`)).toBeVisible();
  expect(within(page).getByRole('heading', { name: '主要功能' })).toBeVisible();
  expect(within(page).getByRole('heading', { name: '使用说明' })).toBeVisible();
  expect(screen.queryByText('Windows Beta')).not.toBeInTheDocument();

  await userEvent.click(within(navigation).getByRole('button', { name: /^任务/ }));
  expect(screen.queryByRole('region', { name: '关于' })).not.toBeInTheDocument();
  expect(screen.getByText('archive.iso')).toBeVisible();
  expect(about).not.toHaveClass('nav-active');
  expect(about).not.toHaveAttribute('aria-current');
});

it('anchors About at the bottom of the sidebar without the Beta footer', async () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    await act(async () => { render(<App />); });
    const navigation = screen.getByRole('navigation', { name: '主导航' });
    const about = within(navigation).getByRole('button', { name: '关于' });
    expect(within(navigation).getAllByRole('button').at(-1)).toBe(about);
    expect(getComputedStyle(about).marginTop).toBe('auto');
    expect(screen.queryByText('Windows Beta')).not.toBeInTheDocument();
  } finally {
    stylesheet.remove();
  }
});

it('highlights a newer release and opens its official download page from About', async () => {
  const update = {
    version: '0.2.0',
    notes: '新增版本更新提示，并优化隐私任务操作体验。',
    downloadUrl: 'https://versadown.aigutta.com/download.html'
  };
  state.updateCheck.mockResolvedValue(update);
  const user = userEvent.setup();
  await act(async () => { render(<App />); });

  const navigation = screen.getByRole('navigation', { name: '主导航' });
  await user.click(await within(navigation).findByRole('button', { name: '有新版本' }));

  const updateLink = await screen.findByRole('button', { name: '下载新版本 0.2.0' });
  expect(updateLink).toHaveTextContent('新版本发布：0.2.0');
  expect(updateLink).toHaveTextContent(update.notes);
  await user.click(updateLink);

  expect(state.openExternalUrl).toHaveBeenCalledWith(update.downloadUrl);
});

it('selects WebView2 caption regions without also registering Tauri JavaScript drag regions', async () => {
  state.windowSnapshot.mockResolvedValue({ active: false, nativeDrag: true });
  const platform = vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
  vi.stubGlobal('CSS', { supports: () => true });
  try {
    await act(async () => { render(<App />); });
    const header = document.querySelector('.app-header')!;
    expect(header).toHaveAttribute('data-native-window-drag', 'true');
    expect(header.querySelector('[data-tauri-drag-region]')).not.toBeInTheDocument();
    expect(header.querySelector('.brand img')).toHaveAttribute('draggable', 'false');
    expect(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }).closest('[data-window-no-drag]')).not.toBeNull();
  } finally {
    vi.unstubAllGlobals();
    platform.mockRestore();
  }
});

it('retains the Tauri drag fallback when WebView2 caption support is unavailable', async () => {
  await act(async () => { render(<App />); });
  expect(document.querySelector('.brand')).toHaveAttribute('data-tauri-drag-region', 'deep');
});

it('keeps title-bar controls outside native drag regions', async () => {
  await act(async () => { render(<App />); });

  expect(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' })).not.toHaveAttribute('data-tauri-drag-region');
  expect(screen.getByRole('button', { name: '最小化' })).not.toHaveAttribute('data-tauri-drag-region');

  fireEvent.mouseDown(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), { button: 0 });
  fireEvent.mouseDown(screen.getByRole('button', { name: '最小化' }), { button: 0 });

  expect(state.startDragging).not.toHaveBeenCalled();
});

it('uses equal square hit targets for every main-window control', () => {
  const styles = readFileSync('src/styles.css', 'utf8');
  expect(styles).toContain('.window-controls{position:relative;z-index:1;display:flex;height:64px;flex:0 0 auto;align-items:center;margin-left:auto}');
  expect(styles).toContain('.window-controls button{display:grid;width:48px;height:48px;flex:0 0 48px;padding:0;place-items:center;border:0;border-radius:8px;');
  expect(styles).not.toContain('.window-controls button{width:34px}');
});

it('uses regular font weight for task tabs and file titles', async () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    state.list.mockResolvedValueOnce([oldTask]);
    render(<App />);

    const taskTitle = await screen.findByText('archive.iso');
    const downloadingTab = screen.getByRole('tab', { name: /下载中/ });

    expect(getComputedStyle(taskTitle).fontWeight).toBe('400');
    expect(getComputedStyle(downloadingTab).fontWeight).toBe('400');
  } finally {
    stylesheet.remove();
  }
});

it('keeps the application title bold as the typography exception', async () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<App />);

    const title = await screen.findByRole('heading', { name: '万能下载' });
    expect(getComputedStyle(title).fontWeight).toBe('700');
  } finally {
    stylesheet.remove();
  }
});

it('shows an unavailable engine and clears it after a successful manual retry', async () => {
  const user = userEvent.setup();
  state.list.mockResolvedValueOnce([]);
  state.getEngineState.mockResolvedValueOnce({
    available: false, retrying: false, error: '下载引擎不可用：找不到 aria2'
  });
  state.retryEngine.mockResolvedValueOnce({ available: true, retrying: false });

  render(<App />);

  expect(await screen.findByText('下载引擎不可用')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '重试下载引擎' }));
  await waitFor(() => expect(screen.queryByText('下载引擎不可用')).not.toBeInTheDocument());
  expect(state.retryEngine).toHaveBeenCalledOnce();
});

it('keeps a live snapshot when the initial list returns an older copy', async () => {
  const initial = deferred<QueueTask[]>();
  state.list.mockReturnValueOnce(initial.promise);
  render(<App />);
  await waitFor(() => expect(state.onTask).toBeDefined());
  act(() => { state.onTask?.(liveTask); });
  await act(async () => { initial.resolve([oldTask]); });

  expect(screen.getByRole('progressbar', { name: 'archive.iso 下载进度' })).toHaveAttribute('aria-valuenow', '40');
  expect(within(screen.getByRole('region', { name: '下载队列' })).getAllByText('archive.iso')).toHaveLength(1);
});

it('keeps Ctrl and Shift task selections through live progress updates', async () => {
  const first: QueueTask = { ...oldTask, id: 'multi-1', name: 'first.iso' };
  const second: QueueTask = { ...oldTask, id: 'multi-2', name: 'second.iso' };
  const third: QueueTask = { ...oldTask, id: 'multi-3', name: 'third.iso' };
  state.list.mockResolvedValue([first, second, third]);
  render(<App />);

  await screen.findByText('first.iso');
  fireEvent.click(screen.getByText('first.iso'));
  fireEvent.click(screen.getByText('second.iso'), { ctrlKey: true });
  fireEvent.click(screen.getByText('third.iso'), { shiftKey: true });
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(3);

  act(() => { state.onTask?.({ ...first, progress: 20, completed_length: 20 }); });
  await waitFor(() => expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(3));
});

it('keeps Ctrl+A task selection after the keys are released and progress updates', async () => {
  const first: QueueTask = { ...oldTask, id: 'select-all-1', name: 'first.iso' };
  const second: QueueTask = { ...oldTask, id: 'select-all-2', name: 'second.iso' };
  state.list.mockResolvedValue([first, second]);
  render(<App />);

  await screen.findByText('first.iso');
  fireEvent.keyDown(document, { key: 'Control', ctrlKey: true });
  fireEvent.keyDown(document, { key: 'a', ctrlKey: true });
  fireEvent.keyUp(document, { key: 'a', ctrlKey: true });
  fireEvent.keyUp(document, { key: 'Control' });
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(2);

  act(() => { state.onTask?.({ ...second, progress: 20, completed_length: 20 }); });
  await waitFor(() => expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(2));
});

it('requests active tasks for the first paint before loading terminal tabs in the background', async () => {
  const terminal = deferred<QueueTask[]>();
  const activeTask: QueueTask = { ...oldTask, id: 'active-startup', name: 'active-startup.iso' };
  const completedTask: QueueTask = { ...oldTask, id: 'completed-startup', name: 'completed-startup.iso', status: 'completed', progress: 100 };
  state.list.mockImplementation((scope?: string) => {
    if (scope === 'active') return Promise.resolve([activeTask]);
    if (scope === 'terminal') return terminal.promise;
    return Promise.resolve([activeTask, completedTask]);
  });
  const user = userEvent.setup();

  render(<App />);

  expect(await screen.findByText('active-startup.iso')).toBeVisible();
  expect(state.list.mock.calls[0]).toEqual(['active']);
  expect(screen.queryByText('completed-startup.iso')).not.toBeInTheDocument();

  await act(async () => { terminal.resolve([completedTask]); });
  await user.click(screen.getByRole('tab', { name: /已完成/ }));

  expect(await screen.findByText('completed-startup.iso')).toBeVisible();
  expect(state.list.mock.calls.map(([scope]) => scope)).toEqual(['active', 'terminal']);
});

it('review shares overlapping task reauthorization requests', async () => {
  const failed = { id: 'overlap', name: '重复授权.mp4', kind: 'cloud', status: 'failed', progress: 0, source: 'cloud://baidu/file', reauthorization_required: true };
  state.list.mockResolvedValue([failed]);
  const begin = deferred<any>();
  state.reauthorizeFailedCloudTask.mockReturnValue(begin.promise);
  render(<App />);
  fireEvent.click(screen.getByRole('tab', { name: /失败/ }));
  const button = await screen.findByRole('button', { name: '重新授权 重复授权.mp4' });
  fireEvent.click(button);
  fireEvent.click(button);
  expect(state.reauthorizeFailedCloudTask).toHaveBeenCalledTimes(1);
  await act(async () => { begin.resolve({ provider: 'baidu', windowLabel: 'auth', instructions: '等待' }); });
  fireEvent.click(button);
  expect(state.reauthorizeFailedCloudTask).toHaveBeenCalledTimes(1);
});

it('shows a native authorization failure and allows the Baidu authorization button to be retried', async () => {
  const failed: QueueTask = { id: 'auth-error', name: 'book.pdf', kind: 'cloud', status: 'failed', progress: 0,
    source: 'cloud://baidu/file', reauthorization_required: true };
  state.list.mockResolvedValue([failed]);
  state.reauthorizeFailedCloudTask.mockRejectedValue({ code: 'storage', message: '授权窗口打开失败，请重试' });
  const user = userEvent.setup();
  render(<App />);
  await user.click(screen.getByRole('tab', { name: /失败/ }));
  await user.click(await screen.findByRole('button', { name: '重新授权 book.pdf' }));
  expect(await screen.findByRole('status')).toHaveTextContent('授权窗口打开失败，请重试');
  await user.click(screen.getByRole('button', { name: '重新授权 book.pdf' }));
  expect(state.reauthorizeFailedCloudTask).toHaveBeenCalledTimes(2);
});

it('shows the duplicate decision when retry returns a queue projection', async () => {
  const failed: QueueTask = { id: 'retry-existing', name: 'book.pdf', kind: 'cloud', status: 'failed', progress: 0 };
  state.list.mockResolvedValue([failed]);
  state.taskAction.mockResolvedValue({ queueTask: failed, duplicate: { task: failed, state: 'overwrite_required' } });
  const user = userEvent.setup();
  render(<App />);
  await user.click(screen.getByRole('tab', { name: /失败/ }));
  await user.click(await screen.findByRole('button', { name: '重新下载 book.pdf' }));
  expect(await screen.findByRole('dialog', { name: '检测到重复下载' })).toBeVisible();
});

it('optimistically queues a refresh and restores its failed state when the action is rejected', async () => {
  const failed: QueueTask = {
    id: 'retry-pending', name: 'retry-pending.zip', kind: 'file', status: 'failed', progress: 12,
    error: '上次下载失败', total_length: 100, completed_length: 12,
  };
  const action = deferred<{ task?: QueueTask }>();
  state.list.mockResolvedValue([failed]);
  state.taskAction.mockReturnValueOnce(action.promise);
  const user = userEvent.setup();
  render(<App />);
  await user.click(screen.getByRole('tab', { name: /失败/ }));

  await user.click(await screen.findByRole('button', { name: '重新下载 retry-pending.zip' }));
  await user.click(screen.getByRole('tab', { name: /下载中/ }));
  expect(screen.getByRole('button', { name: 'retry-pending.zip 排队中' })).toBeEnabled();
  expect(screen.getByText('正在提交下载任务…')).toBeVisible();

  await act(async () => { action.reject(new Error('下载引擎暂时不可用')); });
  await user.click(screen.getByRole('tab', { name: /失败/ }));
  expect(await screen.findByRole('button', { name: '重新下载 retry-pending.zip' })).toBeVisible();
  expect(screen.getByRole('status')).toHaveTextContent('下载引擎暂时不可用');
});

it.each(['event', 'watchdog'])('shows a reauthorized Baidu task after authorization via %s', async (delivery) => {
  vi.useFakeTimers();
  try {
    const failedTask: QueueTask = {
      id: 'baidu-reauthorize-task', name: '需要验证的文件.mp4', kind: 'cloud', status: 'failed', progress: 0,
      source: 'cloud://baidu/file-123', error: '网盘要求完成官方操作验证后重试', reauthorization_required: true,
    };
    const resumedTask: QueueTask = {
      ...failedTask, status: 'downloading', error: undefined, reauthorization_required: false,
    };
    let authorizationCompleted = false;
    state.list.mockImplementation((scope?: string) => {
      if (scope === 'active') return Promise.resolve(authorizationCompleted ? [resumedTask] : []);
      if (scope === 'terminal') return Promise.resolve(authorizationCompleted ? [] : [failedTask]);
      return Promise.resolve(authorizationCompleted ? [resumedTask] : [failedTask]);
    });
    state.reauthorizeFailedCloudTask.mockResolvedValue({
      provider: 'baidu', windowLabel: 'cloud-auth-baidu', instructions: '请在官方网盘窗口完成登录',
    });
    state.completeCloudAuthorization.mockImplementation(() => {
      authorizationCompleted = true;
      return Promise.resolve({ provider: 'baidu', id: 'baidu-user', displayName: '百度用户', authMethod: 'web' });
    });

    render(<App />);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); await vi.advanceTimersByTimeAsync(0); });
    fireEvent.click(screen.getByRole('tab', { name: /失败/ }));
    fireEvent.click(screen.getByRole('button', { name: '重新授权 需要验证的文件.mp4' }));
    await act(async () => { await Promise.resolve(); });

    if (delivery === 'event') {
      authorizationCompleted = true;
      state.getCompletedCloudAuthorization.mockResolvedValue({ provider: 'baidu', id: 'baidu-user', displayName: '百度用户', authMethod: 'web' });
      await act(async () => { state.listeners.get('cloud-authorization-state')?.({ payload: { provider: 'baidu', state: 'Ready', account: { provider: 'baidu', id: 'baidu-user', displayName: '百度用户', authMethod: 'web' } } }); });
      expect(state.completeCloudAuthorization).not.toHaveBeenCalled();
    } else {
      await act(async () => { await vi.advanceTimersByTimeAsync(9_999); });
      expect(state.completeCloudAuthorization).not.toHaveBeenCalled();
      await act(async () => { await vi.advanceTimersByTimeAsync(1); });
    }
    expect(screen.getByRole('status')).toHaveTextContent('授权成功，正在恢复受影响的下载任务');

    fireEvent.click(screen.getByRole('tab', { name: /下载中/ }));
    expect(screen.getByTitle('需要验证的文件.mp4')).toBeVisible();
    expect(screen.getByRole('progressbar', { name: '需要验证的文件.mp4 下载进度' })).toHaveAttribute('aria-valuenow', '0');
  } finally {
    vi.useRealTimers();
  }
});

it('refreshes a task after automatic password renewal without polling a closed authorization window', async () => {
  const failed: QueueTask = { id: 'automatic-login', name: '自动恢复.mp4', kind: 'cloud',
    status: 'failed', progress: 0, source: 'cloud://baidu/file-1', reauthorization_required: true };
  let recovered = false;
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active'
    ? recovered ? [{ ...failed, status: 'downloading', reauthorization_required: false }] : []
    : recovered ? [] : [failed]));
  state.reauthorizeFailedCloudTask.mockImplementation(async () => {
    recovered = true;
    return { provider: 'baidu', windowLabel: '', completed: true, instructions: '登录已自动恢复，正在继续下载' };
  });
  const user = userEvent.setup();
  render(<App />);
  await user.click(screen.getByRole('tab', { name: /失败/ }));
  await user.click(await screen.findByRole('button', { name: '重新授权 自动恢复.mp4' }));
  await screen.findByText('登录已自动恢复，正在继续下载');
  await user.click(screen.getByRole('tab', { name: /下载中/ }));
  expect(await screen.findByTitle('自动恢复.mp4')).toBeVisible();
  expect(state.completeCloudAuthorization).not.toHaveBeenCalled();
});

it('opens a unified task window and parses a typed download link immediately', async () => {
  const user = userEvent.setup();
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([{ input: 'magnet:?xt=urn:btih:abc', valid: true, kind: 'magnet' }]);
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'torrent-batch', title: 'collection', requiresConfirmation: true,
      resources: [{ id: 'file-1', name: 'movie.mkv', size: '1 GB', selected: true }], processing: [],
    },
  });
  state.confirm.mockResolvedValueOnce({ id: 'torrent-batch', name: 'collection', kind: 'torrent', status: 'downloading', progress: 0 });

  render(<App />);
  await user.type(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), 'magnet:?xt=urn:btih:abc{Enter}');
  const dialog = screen.getByRole('dialog', { name: '新建下载' });
  expect(dialog).toHaveClass('batch-download-dialog', 'modal');
  expect(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' })).toBeDisabled();
  expect(await within(dialog).findByText('collection')).toBeVisible();
  await user.click(within(dialog).getByRole('button', { name: '新建任务' }));

  await waitFor(() => expect(screen.getByRole('tab', { name: /下载中/ })).toHaveAttribute('aria-selected', 'true'));
  expect(state.parse).toHaveBeenCalledWith('magnet:?xt=urn:btih:abc', undefined, true);
});

it('shows a structured Baidu share preflight error in the batch task dialog', async () => {
  const user = userEvent.setup();
  const share = 'https://pan.baidu.com/s/1example?pwd=7gfb';
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([{ input: share, valid: true, kind: 'cloud-share' }]);
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://baidu/plan', title: 'archive.7z', requiresConfirmation: true,
      resources: [{ id: 'file-1', name: 'archive.7z', size: '1 GB', selected: true }], processing: [],
    },
  });
  state.confirmCloud.mockRejectedValueOnce({
    code: 'web_auth_required',
    message: '网盘要求完成官方操作验证后重试',
  });

  render(<App />);
  await user.type(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), `${share}{Enter}`);
  const dialog = await screen.findByRole('dialog', { name: '新建下载' });
  await within(dialog).findByText('archive.7z');
  await user.click(within(dialog).getByRole('button', { name: '新建任务' }));

  expect(await within(dialog).findByRole('alert')).toHaveTextContent('网盘要求完成官方操作验证后重试');
  expect(within(dialog).queryByText('[object Object]')).not.toBeInTheDocument();
});

it('keeps a Baidu share task unsubmitted and asks for login when authorization is missing', async () => {
  const user = userEvent.setup();
  const share = 'https://pan.baidu.com/s/1example?pwd=7gfb';
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([{ input: share, valid: true, kind: 'cloud-share' }]);
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://baidu/plan', title: 'archive.7z', requiresConfirmation: true,
      resources: [{ id: 'file-1', name: 'archive.7z', size: '1 GB', selected: true }], processing: [],
    },
  });
  state.confirmCloud.mockRejectedValueOnce({
    code: 'not_authorized',
    message: '尚未授权此网盘，请登录后使用',
  });

  render(<App />);
  await user.type(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), `${share}{Enter}`);
  const dialog = await screen.findByRole('dialog', { name: '新建下载' });
  await within(dialog).findByText('archive.7z');
  await user.click(within(dialog).getByRole('button', { name: '新建任务' }));

  expect(await within(dialog).findByRole('alert')).toHaveTextContent('尚未授权此网盘，请登录后使用');
  expect(state.confirmCloud).toHaveBeenCalledTimes(1);
  expect(dialog).toBeVisible();
});

it('uses the supplied dark SVG icon and vector icons in the primary navigation', async () => {
  const previousTheme = getThemeMode();
  setThemeMode('deep-ocean');
  const app = render(<App />);

  try {
    await screen.findByRole('heading', { name: '万能下载' });
    const brandMark = document.querySelector('.brand-mark');
    expect(brandMark).toHaveAttribute('data-icon-variant', 'dark');
    expect(brandMark?.querySelector('img')).toHaveAttribute('src', expect.stringContaining('dark_mint.svg'));

    const navigation = screen.getByRole('navigation', { name: '主导航' });
    for (const button of within(navigation).getAllByRole('button')) {
      expect(button.querySelector('.nav-icon svg')).toBeInTheDocument();
    }
    expect(navigation).not.toHaveTextContent('↓');
    expect(navigation).not.toHaveTextContent('☁');
    expect(navigation).not.toHaveTextContent('⚙');
  } finally {
    app.unmount();
    setThemeMode(previousTheme);
  }
});

it('removes recycle controls without adding page-wide start or pause links', async () => {
  const active = { id: 'active-task', name: 'active.zip', kind: 'file', status: 'downloading' as const, progress: 25 };
  const recycled = { id: 'recycled-task', name: 'removed.zip', kind: 'file', status: 'recycled' as const, progress: 100 };
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'active' ? [active] : scope === 'terminal' ? [recycled] : [active, recycled]
  ));
  render(<App />);

  const speed = await screen.findByLabelText('当前总下载速度');
  await screen.findByText('active.zip');
  expect(screen.queryByRole('button', { name: /全部开始|全部暂停/ })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '清空' })).not.toBeInTheDocument();

  expect(screen.queryByRole('tab', { name: /回收站/ })).not.toBeInTheDocument();
  expect(screen.getByRole('tablist', { name: '下载任务' }).nextElementSibling).toBe(speed);
  expect(screen.queryByRole('button', { name: /全部暂停/ })).not.toBeInTheDocument();
});

it('renders the total download speed in bold', async () => {
  state.list.mockResolvedValueOnce([
    { id: 'speed-task', name: 'speed.zip', kind: 'file', status: 'downloading', progress: 25, download_speed: 409_600 }
  ]);
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<App />);
    const speed = await screen.findByLabelText('当前总下载速度');
    expect(getComputedStyle(within(speed).getByText('400.0 KB/s')).fontWeight).toBe('700');
  } finally {
    stylesheet.remove();
  }
});

it('does not offer manual ordering from the task sort menu', async () => {
  const user = userEvent.setup();
  render(<App />);

  await user.click(await screen.findByRole('button', { name: '排序任务' }));
  const menu = screen.getByRole('menu', { name: '任务排序' });
  expect(within(menu).queryByRole('menuitemradio', { name: '按手动拖动顺序' })).not.toBeInTheDocument();
});

it('uses status and task timestamps for each tab by default, ignoring saved sort choices', async () => {
  const tasks: QueueTask[] = [
    { ...oldTask, id: 'done-late-created', name: 'done-late-created.zip', status: 'completed', created_at: 500, completed_at: 600 },
    { ...oldTask, id: 'failed-early-created', name: 'failed-early-created.zip', status: 'failed', created_at: 250, updated_at: 900 },
    { ...oldTask, id: 'active-old', name: 'active-old.zip', created_at: 100 },
    { ...oldTask, id: 'done-early-created', name: 'done-early-created.zip', status: 'completed', created_at: 200, completed_at: 1000 },
    { ...oldTask, id: 'active-new', name: 'active-new.zip', created_at: 400 },
    { ...oldTask, id: 'failed-late-created', name: 'failed-late-created.zip', status: 'failed', created_at: 300, updated_at: 700 },
  ];
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'active' ? tasks.filter((task) => task.status === 'downloading')
      : scope === 'terminal' ? tasks.filter((task) => task.status !== 'downloading') : tasks,
  ));
  window.localStorage.setItem('task-list-order-v1', JSON.stringify({
    sorts: { all: 'name-asc', active: 'name-asc', completed: 'created-desc', failed: 'created-desc' },
    orders: { all: tasks.map((task) => task.id) },
  }));
  const user = userEvent.setup();
  render(<App />);

  const queue = await screen.findByRole('region', { name: '下载队列' });
  const visibleNames = () => Array.from(queue.querySelectorAll<HTMLElement>('.task-copy b')).map((item) => item.textContent);
  await waitFor(() => expect(visibleNames()).toHaveLength(tasks.length));
  expect(visibleNames()).toEqual([
    'active-new.zip', 'active-old.zip', 'done-late-created.zip',
    'failed-late-created.zip', 'failed-early-created.zip', 'done-early-created.zip',
  ]);

  await user.click(screen.getByRole('tab', { name: /下载中/ }));
  expect(visibleNames()).toEqual(['active-new.zip', 'active-old.zip']);
  await user.click(screen.getByRole('tab', { name: /已完成/ }));
  expect(visibleNames()).toEqual(['done-early-created.zip', 'done-late-created.zip']);
  await user.click(screen.getByRole('tab', { name: /失败/ }));
  expect(visibleNames()).toEqual(['failed-early-created.zip', 'failed-late-created.zip']);
});

it('keeps user sorting in memory and restores the all-tab default when the launcher opens it', async () => {
  const tasks: QueueTask[] = [
    { ...oldTask, id: 'terminal-new', name: 'alpha.zip', status: 'completed', created_at: 300, completed_at: 400 },
    { ...oldTask, id: 'active-old', name: 'zeta.zip', created_at: 100 },
  ];
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'active' ? tasks.filter((task) => task.status === 'downloading')
      : scope === 'terminal' ? tasks.filter((task) => task.status !== 'downloading') : tasks,
  ));
  window.localStorage.removeItem('task-list-order-v1');
  const user = userEvent.setup();
  render(<App />);
  const queue = await screen.findByRole('region', { name: '下载队列' });
  const visibleNames = () => Array.from(queue.querySelectorAll<HTMLElement>('.task-copy b')).map((item) => item.textContent);
  await waitFor(() => expect(visibleNames()).toEqual(['zeta.zip', 'alpha.zip']));

  await user.click(screen.getByRole('button', { name: '排序任务' }));
  await user.click(screen.getByRole('menuitemradio', { name: '按文件名称从 A 到 Z' }));
  expect(visibleNames()).toEqual(['alpha.zip', 'zeta.zip']);
  await user.click(screen.getByRole('tab', { name: /下载中/ }));
  await user.click(screen.getByRole('tab', { name: /全部/ }));
  expect(visibleNames()).toEqual(['alpha.zip', 'zeta.zip']);
  expect(window.localStorage.getItem('task-list-order-v1')).toBeNull();

  await waitFor(() => expect(state.listeners.get('launcher-open-all-tasks')).toBeDefined());
  act(() => { state.listeners.get('launcher-open-all-tasks')?.({ payload: undefined }); });
  expect(visibleNames()).toEqual(['zeta.zip', 'alpha.zip']);
  expect(window.localStorage.getItem('task-list-order-v1')).toBeNull();
});

it('sorts only the active task tab from the menu beside total speed', async () => {
  const active = [
    { id: 'active-old', name: 'alpha.zip', kind: 'file', status: 'downloading' as const, progress: 80, total_length: 800, created_at: 100 },
    { id: 'active-new', name: 'zeta.zip', kind: 'file', status: 'downloading' as const, progress: 20, total_length: 200, created_at: 200 },
  ];
  const terminal = [
    { id: 'done-old', name: 'beta.zip', kind: 'file', status: 'completed' as const, progress: 100, total_length: 400, created_at: 100 },
    { id: 'done-new', name: 'delta.zip', kind: 'file', status: 'completed' as const, progress: 100, total_length: 300, created_at: 300 },
  ];
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'active' ? active : scope === 'terminal' ? terminal : [...active, ...terminal]
  ));
  const user = userEvent.setup();
  render(<App />);

  await user.click(screen.getByRole('tab', { name: /下载中/ }));
  const queue = await screen.findByRole('region', { name: '下载队列' });
  const visibleNames = () => Array.from(queue.querySelectorAll<HTMLElement>('.task-copy b')).map((item) => item.textContent);
  await waitFor(() => expect(visibleNames()).toEqual(['zeta.zip', 'alpha.zip']));

  await user.click(screen.getByRole('button', { name: '排序任务' }));
  expect(screen.getByRole('menu', { name: '任务排序' })).toBeVisible();
  await user.click(screen.getByRole('menuitemradio', { name: '按文件名称从 A 到 Z' }));
  expect(visibleNames()).toEqual(['alpha.zip', 'zeta.zip']);

  await user.click(screen.getByRole('button', { name: '排序任务' }));
  await user.click(screen.getByRole('menuitemradio', { name: '按当前进度从低到高' }));
  expect(visibleNames()).toEqual(['zeta.zip', 'alpha.zip']);

  await user.click(screen.getByRole('button', { name: '排序任务' }));
  await user.click(screen.getByRole('menuitemradio', { name: '按文件大小从大到小' }));
  expect(visibleNames()).toEqual(['alpha.zip', 'zeta.zip']);

  await user.click(screen.getByRole('tab', { name: /已完成/ }));
  expect(visibleNames()).toEqual(['delta.zip', 'beta.zip']);
});

it('keeps a newly added download in the explicitly selected sort order', async () => {
  window.localStorage.removeItem('task-list-order-v1');
  const active = [
    { id: 'existing-zeta', name: 'zeta.zip', kind: 'file' as const, status: 'downloading' as const, progress: 20 },
    { id: 'existing-alpha', name: 'alpha.zip', kind: 'file' as const, status: 'downloading' as const, progress: 80 },
  ];
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'active' ? active : scope === 'terminal' ? [] : active
  ));
  state.parse.mockResolvedValueOnce({
    task: { id: 'new-middle', name: 'middle.zip', kind: 'file', status: 'queued', progress: 0 },
  });
  const user = userEvent.setup();
  render(<App />);

  const queue = await screen.findByRole('region', { name: '下载队列' });
  const visibleNames = () => Array.from(queue.querySelectorAll<HTMLElement>('.task-copy b')).map((item) => item.textContent);
  await user.click(screen.getByRole('button', { name: '排序任务' }));
  await user.click(screen.getByRole('menuitemradio', { name: '按文件名称从 A 到 Z' }));
  expect(visibleNames()).toEqual(['alpha.zip', 'zeta.zip']);

  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  act(() => { state.listeners.get('launcher-parse')?.({ payload: 'https://example.test/middle.zip' }); });

  await waitFor(() => expect(visibleNames()).toEqual(['alpha.zip', 'middle.zip', 'zeta.zip']));
  await user.click(screen.getByRole('button', { name: '排序任务' }));
  expect(screen.getByRole('menuitemradio', { name: '按文件名称从 A 到 Z' })).toHaveAttribute('aria-checked', 'true');
  act(() => { state.onTask?.({ id: 'new-middle', name: 'middle.zip', kind: 'file', status: 'completed', progress: 100 }); });
  await waitFor(() => expect(visibleNames()).toEqual(['alpha.zip', 'middle.zip', 'zeta.zip']));
  window.localStorage.removeItem('task-list-order-v1');
});

it.each([
  { toggleOff: '显示普通任务', keepOn: '显示隐私任务', visible: 'private.zip', hidden: 'ordinary.zip' },
  { toggleOff: '显示隐私任务', keepOn: '显示普通任务', visible: 'ordinary.zip', hidden: 'private.zip' },
])('keeps at least one task visibility option selected when toggling $toggleOff', async ({ toggleOff, keepOn, visible, hidden }) => {
  const ordinaryTask: QueueTask = { ...oldTask, id: 'ordinary-task', name: 'ordinary.zip' };
  const privateTask: QueueTask = { ...oldTask, id: 'private-task', name: 'private.zip', is_private: true };
  state.privacyVisibility.mockResolvedValue({ enabled: true, unlocked: true });
  state.list.mockResolvedValue([ordinaryTask, privateTask]);
  const user = userEvent.setup();
  render(<App />);

  const queue = await screen.findByRole('region', { name: '下载队列' });
  expect(await screen.findByRole('button', { name: '锁定隐私任务' })).toBeVisible();
  expect(screen.queryByRole('combobox', { name: '筛选任务' })).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: '排序任务' }));
  const menu = screen.getByRole('menu', { name: '任务排序' });
  const toggledOption = within(menu).getByRole('menuitemcheckbox', { name: toggleOff });
  const remainingOption = within(menu).getByRole('menuitemcheckbox', { name: keepOn });
  expect(within(menu).getAllByRole('menuitemcheckbox')).toHaveLength(2);
  expect(toggledOption).toHaveAttribute('aria-checked', 'true');
  expect(remainingOption).toHaveAttribute('aria-checked', 'true');
  expect(within(queue).getByTitle(visible)).toBeVisible();
  expect(within(queue).getByTitle(hidden)).toBeVisible();

  await user.click(toggledOption);
  expect(menu).toBeVisible();
  expect(toggledOption).toHaveAttribute('aria-checked', 'false');
  expect(remainingOption).toHaveAttribute('aria-checked', 'true');
  expect(within(queue).getByTitle(visible)).toBeVisible();
  expect(within(queue).queryByTitle(hidden)).not.toBeInTheDocument();

  await user.click(remainingOption);
  expect(remainingOption).toHaveAttribute('aria-checked', 'true');
  expect(within(queue).getByTitle(visible)).toBeVisible();

  await user.click(toggledOption);
  expect(toggledOption).toHaveAttribute('aria-checked', 'true');
  expect(remainingOption).toHaveAttribute('aria-checked', 'true');
  expect(within(queue).getByTitle(visible)).toBeVisible();
  expect(within(queue).getByTitle(hidden)).toBeVisible();
  await user.keyboard('{Escape}');
  expect(screen.queryByRole('menu')).not.toBeInTheDocument();
});

it('does not expose private tasks or visibility options while privacy is locked', async () => {
  state.privacyVisibility.mockResolvedValue({ enabled: true, unlocked: false });
  state.list.mockResolvedValue([
    { ...oldTask, id: 'ordinary-task', name: 'ordinary.zip' },
    { ...oldTask, id: 'private-task', name: 'private.zip', is_private: true },
  ]);
  const user = userEvent.setup();
  render(<App />);
  await screen.findByRole('button', { name: '解锁隐私任务' });
  await user.click(screen.getByRole('button', { name: '排序任务' }));
  expect(within(screen.getByRole('menu')).queryByRole('menuitemcheckbox')).not.toBeInTheDocument();
  const queue = screen.getByRole('region', { name: '下载队列' });
  expect(within(queue).getByTitle('ordinary.zip')).toBeVisible();
  expect(within(queue).queryByTitle('private.zip')).not.toBeInTheDocument();
});

it.each([{ is_private: true }, { isPrivate: true }])('clears the open details after a private move response %j while locked', async (privacyFields) => {
  const ordinaryTask: QueueTask = { ...oldTask, id: 'move-private-now', name: '马上隐藏.zip' };
  state.getSettings.mockResolvedValue({
    downloadDirectory: 'C:\\Downloads', launcherVisibility: 'always',
    proxy: { enabled: false, protocol: 'http', host: '', port: 7890, username: '', password: '' },
    magnetMetadata: { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' },
    aria2: { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 5, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false },
    privacyDownloadEnabled: true,
  });
  state.privacyVisibility.mockResolvedValue({ enabled: true, unlocked: false });
  state.list.mockResolvedValue([ordinaryTask]);
  state.setTaskPrivacy.mockResolvedValue({ ...ordinaryTask, ...privacyFields });
  const user = userEvent.setup();
  render(<App />);

  await screen.findByRole('button', { name: '解锁隐私任务' });
  await user.click(screen.getByRole('button', { name: '更多操作 马上隐藏.zip' }));
  await user.click(screen.getByRole('menuitem', { name: '查看任务详情' }));
  expect(screen.getByRole('dialog', { name: '任务详情' })).toBeVisible();

  fireEvent.contextMenu(screen.getAllByText('马上隐藏.zip')[0]);
  await user.click(screen.getByRole('menuitem', { name: '移入隐私下载' }));

  await waitFor(() => expect(state.setTaskPrivacy).toHaveBeenCalledWith('move-private-now', true));
  await waitFor(() => expect(screen.queryByTitle('马上隐藏.zip')).not.toBeInTheDocument());
  expect(screen.queryByRole('dialog', { name: '任务详情' })).not.toBeInTheDocument();
  act(() => { state.onTask?.(ordinaryTask); });
  expect(screen.getByTitle('马上隐藏.zip')).toBeVisible();
  expect(screen.queryByRole('dialog', { name: '任务详情' })).not.toBeInTheDocument();
});

it('uses the shared line icon and input treatment when unlocking private tasks', async () => {
  state.privacyVisibility.mockResolvedValue({ enabled: true, unlocked: false });
  const user = userEvent.setup();
  render(<App />);

  const unlockButton = await screen.findByRole('button', { name: '解锁隐私任务' });
  expect(unlockButton.querySelector('svg')).not.toBeNull();
  expect(unlockButton).not.toHaveTextContent('🔒');
  await user.click(unlockButton);

  const passwordInput = await screen.findByLabelText('隐私模式密码');
  expect(passwordInput.closest('.cloud-operation-editor')).not.toBeNull();
  const dialog = screen.getByRole('dialog', { name: '输入隐私密码解锁' });
  expect(within(dialog).getByRole('heading', { name: '输入隐私密码解锁' })).toBeVisible();
  expect(within(dialog).queryByText('隐私模式密码')).not.toBeInTheDocument();
  expect(passwordInput).toHaveFocus();
  await user.click(within(dialog).getByRole('button', { name: '取消' }));
  expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
});

it('places the privacy unlock modal above the global search surface', async () => {
  state.privacyVisibility.mockResolvedValue({ enabled: true, unlocked: false });
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const user = userEvent.setup();
    await act(async () => { render(<App />); });

    await user.click(await screen.findByRole('button', { name: '解锁隐私任务' }));
    const dialog = await screen.findByRole('dialog', { name: '输入隐私密码解锁' });
    const backdrop = dialog.parentElement;
    const searchSurface = screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }).closest('.header-input-shell');

    expect(backdrop).toHaveClass('modal-backdrop');
    expect(searchSurface).not.toBeNull();
    expect(Number(getComputedStyle(backdrop!).zIndex)).toBeGreaterThan(Number(getComputedStyle(searchSurface!).zIndex));
  } finally {
    stylesheet.remove();
  }
});

it('places every application modal backdrop above the global search surface', async () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  const modalBackdrops = ['modal-backdrop new-download-backdrop', 'cloud-confirm-backdrop'].map((className) => {
    const backdrop = document.createElement('div');
    backdrop.className = className;
    document.body.appendChild(backdrop);
    return backdrop;
  });
  try {
    await act(async () => { render(<App />); });
    const searchSurface = screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }).closest('.header-input-shell');

    expect(searchSurface).not.toBeNull();
    for (const backdrop of modalBackdrops) {
      expect(Number(getComputedStyle(backdrop).zIndex)).toBeGreaterThan(Number(getComputedStyle(searchSurface!).zIndex));
    }
  } finally {
    modalBackdrops.forEach((backdrop) => backdrop.remove());
    stylesheet.remove();
  }
});

it('does not expose a page-wide start action when paused tasks exist', async () => {
  const pausedTask: QueueTask = { ...oldTask, id: 'paused-task', status: 'paused' };
  const downloadingTask: QueueTask = { ...oldTask, id: 'downloading-task', status: 'downloading' };
  state.list.mockResolvedValueOnce([pausedTask, downloadingTask]);
  render(<App />);
  await waitFor(() => expect(document.querySelector('[data-task-id="paused-task"]')).toBeInTheDocument());
  expect(screen.queryByRole('button', { name: /全部开始|全部暂停/ })).not.toBeInTheDocument();
  expect(state.taskAction).not.toHaveBeenCalled();
});

it('opens one batch window and parses every torrent dropped on the global input', async () => {
  const first = {
    taskId: 'torrent-first', title: 'first.torrent', requiresConfirmation: true, isBitTorrent: true,
    resources: [{ id: '1', name: 'first/file-one.bin', size: '1 MB', selected: true }], processing: []
  };
  const second = {
    taskId: 'torrent-second', title: 'second.torrent', requiresConfirmation: true, isBitTorrent: true,
    resources: [{ id: '2', name: 'second/file-two.bin', size: '2 MB', selected: true }], processing: []
  };
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([
    { input: 'C:\\BT\\first.torrent', valid: true, kind: 'torrent' },
    { input: 'C:\\BT\\second.TORRENT', valid: true, kind: 'torrent' },
  ]);
  state.parse.mockResolvedValueOnce({ confirmation: first }).mockResolvedValueOnce({ confirmation: second });
  render(<App />);
  await waitFor(() => expect(state.dragDropHandler).toBeDefined());
  const searchTarget = screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }).closest('label')!;
  vi.spyOn(searchTarget, 'getBoundingClientRect').mockReturnValue({ x: 100, y: 20, left: 100, top: 20, right: 500, bottom: 70, width: 400, height: 50, toJSON: () => ({}) });
  const position = (x: number, y: number) => ({ toLogical: () => ({ x, y }) });

  await act(async () => { state.dragDropHandler?.({ payload: { type: 'drop', paths: ['C:\\BT\\ignored.torrent'], position: position(10, 10) } }); });
  expect(state.parse).not.toHaveBeenCalled();

  await act(async () => { state.dragDropHandler?.({ payload: { type: 'drop', paths: ['C:\\BT\\first.torrent', 'C:\\BT\\notes.txt', 'C:\\BT\\second.TORRENT'], position: position(200, 40) } }); });
  await waitFor(() => expect(state.parse).toHaveBeenCalledWith('C:\\BT\\first.torrent', undefined, true));
  await waitFor(() => expect(state.parse).toHaveBeenCalledWith('C:\\BT\\second.TORRENT', undefined, true));
  expect(state.parse).toHaveBeenCalledTimes(2);
  const dialog = await screen.findByRole('dialog', { name: '新建下载' });
  expect(dialog).toHaveTextContent('first.torrent');
  expect(dialog).toHaveTextContent('second.torrent');
});

it('shows every visible task in all and keeps failed tasks out of downloading', async () => {
  const downloading: QueueTask = { ...oldTask, id: 'all-downloading', name: '正在下载.zip', status: 'downloading' };
  const completed: QueueTask = { ...oldTask, id: 'all-completed', name: '已经完成.zip', status: 'completed', progress: 100 };
  const failed: QueueTask = { ...oldTask, id: 'all-failed', name: '下载失败.zip', status: 'failed', progress: 40 };
  const recycled: QueueTask = { ...oldTask, id: 'all-recycled', name: '旧回收记录.zip', status: 'recycled', progress: 100 };
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'active' ? [downloading] : scope === 'terminal' ? [completed, failed, recycled] : [downloading, completed, failed, recycled],
  ));
  const user = userEvent.setup();
  render(<App />);

  const allTab = await screen.findByRole('tab', { name: /全部/ });
  expect(allTab).toHaveAttribute('aria-selected', 'true');
  expect(screen.getByText('正在下载.zip')).toBeVisible();
  expect(await screen.findByText('已经完成.zip')).toBeVisible();
  expect(await screen.findByText('下载失败.zip')).toBeVisible();
  expect(screen.queryByText('旧回收记录.zip')).not.toBeInTheDocument();
  expect(screen.queryByRole('tab', { name: /回收站/ })).not.toBeInTheDocument();

  await user.click(screen.getByRole('tab', { name: /下载中/ }));
  expect(screen.getByText('正在下载.zip')).toBeVisible();
  expect(screen.queryByText('已经完成.zip')).not.toBeInTheDocument();
  expect(screen.queryByText('下载失败.zip')).not.toBeInTheDocument();

  await user.click(screen.getByRole('tab', { name: /已完成/ }));
  expect(screen.getByText('已经完成.zip')).toBeVisible();

  await user.click(screen.getByRole('tab', { name: /失败/ }));
  expect(screen.getByText('下载失败.zip')).toBeVisible();
});

it('uses permanent deletion for an active task instead of only cancelling it', async () => {
  const task: QueueTask = { ...oldTask, id: 'delete-active', name: 'active-delete.zip' };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [task] : []));
  state.deleteTasks.mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<App />);

  fireEvent.contextMenu(await screen.findByText('active-delete.zip'));
  await user.click(screen.getByRole('menuitem', { name: '删除任务' }));
  const dialog = screen.getByRole('dialog', { name: '彻底删除任务' });
  const deleteFiles = within(dialog).getByRole('checkbox', { name: '同时删除下载文件' });
  if ((deleteFiles as HTMLInputElement).checked) await user.click(deleteFiles);
  await user.click(within(dialog).getByRole('button', { name: '彻底删除任务' }));

  await waitFor(() => expect(state.deleteTasks).toHaveBeenCalledWith(['delete-active'], false));
  expect(state.taskAction).not.toHaveBeenCalled();
});

it('selects only the current tab tasks with Ctrl+A across all four task tabs', async () => {
  const groups = [
    { tab: /全部/, statuses: ['downloading', 'failed', 'completed'] }, { tab: /下载中/, statuses: ['downloading'] },
    { tab: /已完成/, statuses: ['completed'] }, { tab: /失败/, statuses: ['failed'] },
  ] as const;
  const tasks = ['downloading', 'failed', 'completed', 'recycled'].flatMap((status) => [1, 2].map(index => ({
    ...oldTask, id: `${status}-${index}`, name: `${status}-${index}.zip`, status,
  })));
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'active'
      ? tasks.filter((task) => task.status === 'downloading')
      : scope === 'terminal'
        ? tasks.filter((task) => task.status === 'completed' || task.status === 'failed' || task.status === 'recycled')
        : tasks
  ));
  const user = userEvent.setup();
  render(<App />);
  await screen.findByText('downloading-1.zip');
  for (const { tab, statuses } of groups) {
    await user.click(screen.getByRole('tab', { name: tab }));
    fireEvent.keyDown(document, { key: 'a', ctrlKey: true });
    const selected = [...document.querySelectorAll('.task-row[aria-selected="true"]')];
    expect(selected.map(row => row.getAttribute('data-task-id')).sort()).toEqual(statuses.flatMap((status) => [`${status}-1`, `${status}-2`]).sort());
  }
});

it('copies all selected task links to the clipboard on separate lines', async () => {
  const first: QueueTask = { ...oldTask, id: 'copy-1', name: 'first.zip', source: 'https://example.com/first.zip' };
  const second: QueueTask = { ...oldTask, id: 'copy-2', name: 'second.zip', source: 'magnet:?xt=urn:btih:SECOND' };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [first, second] : []));
  state.clipboardWrite.mockResolvedValue(undefined);
  render(<App />);

  fireEvent.click(await screen.findByText('first.zip'));
  fireEvent.click(screen.getByText('second.zip'), { ctrlKey: true });
  fireEvent.contextMenu(screen.getByText('first.zip'));
  fireEvent.click(screen.getByRole('menuitem', { name: '复制下载链接' }));

  await waitFor(() => expect(state.clipboardWrite).toHaveBeenCalledOnce());
  const copiedLines = String(state.clipboardWrite.mock.calls[0][0]).split('\n');
  expect(copiedLines).toHaveLength(2);
  expect(copiedLines).toEqual(expect.arrayContaining(['https://example.com/first.zip', 'magnet:?xt=urn:btih:SECOND']));
});

it('starts every selected paused task from the multi-selection menu', async () => {
  const first: QueueTask = { ...oldTask, id: 'resume-1', name: 'first-paused.zip', status: 'paused' };
  const second: QueueTask = { ...oldTask, id: 'resume-2', name: 'second-paused.zip', status: 'paused' };
  const firstAction = deferred<{ task?: QueueTask }>();
  const secondAction = deferred<{ task?: QueueTask }>();
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [first, second] : []));
  state.taskAction.mockImplementation((id: string) => id === first.id ? firstAction.promise : secondAction.promise);
  render(<App />);

  fireEvent.click(await screen.findByText('first-paused.zip'));
  fireEvent.click(screen.getByText('second-paused.zip'), { ctrlKey: true });
  fireEvent.contextMenu(screen.getByText('first-paused.zip'));
  fireEvent.click(screen.getByRole('menuitem', { name: '开始下载' }));

  await waitFor(() => {
    expect(state.taskAction).toHaveBeenCalledWith('resume-1', 'resume');
    expect(state.taskAction).toHaveBeenCalledWith('resume-2', 'resume');
  });
  expect(screen.getByRole('button', { name: 'first-paused.zip 排队中' })).toBeEnabled();
  expect(screen.getByRole('button', { name: 'second-paused.zip 排队中' })).toBeEnabled();
  await act(async () => {
    firstAction.resolve({ task: { ...first, status: 'downloading' } });
    secondAction.resolve({ task: { ...second, status: 'downloading' } });
  });
});

it('deletes every selected active task from the multi-selection menu', async () => {
  const first: QueueTask = { ...oldTask, id: 'delete-1', name: 'first-delete.zip' };
  const second: QueueTask = { ...oldTask, id: 'delete-2', name: 'second-delete.zip' };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [first, second] : []));
  state.deleteTasks.mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<App />);

  fireEvent.click(await screen.findByText('first-delete.zip'));
  fireEvent.click(screen.getByText('second-delete.zip'), { ctrlKey: true });
  fireEvent.contextMenu(screen.getByText('first-delete.zip'));
  await user.click(screen.getByRole('menuitem', { name: '删除' }));
  const dialog = screen.getByRole('dialog', { name: '彻底删除任务' });
  const deleteFiles = within(dialog).getByRole('checkbox', { name: '同时删除下载文件' });
  if ((deleteFiles as HTMLInputElement).checked) await user.click(deleteFiles);
  await user.click(within(dialog).getByRole('button', { name: '彻底删除任务' }));

  await waitFor(() => expect(state.deleteTasks).toHaveBeenCalledTimes(1));
  expect(new Set(state.deleteTasks.mock.calls[0]?.[0])).toEqual(new Set(['delete-1', 'delete-2']));
  expect(state.deleteTasks.mock.calls[0]?.[1]).toBe(false);
  expect(state.taskAction).not.toHaveBeenCalled();
});

it('pauses every selected running task from the multi-selection menu', async () => {
  const first: QueueTask = { ...oldTask, id: 'pause-1', name: 'first-running.zip' };
  const second: QueueTask = { ...oldTask, id: 'pause-2', name: 'second-running.zip' };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [first, second] : []));
  state.taskAction.mockImplementation((id: string) => Promise.resolve({ task: { ...(id === first.id ? first : second), status: 'paused' } }));
  render(<App />);

  fireEvent.click(await screen.findByText('first-running.zip'));
  fireEvent.click(screen.getByText('second-running.zip'), { ctrlKey: true });
  fireEvent.contextMenu(screen.getByText('first-running.zip'));
  fireEvent.click(screen.getByRole('menuitem', { name: '暂停' }));

  await waitFor(() => {
    expect(state.taskAction).toHaveBeenCalledWith('pause-1', 'pause');
    expect(state.taskAction).toHaveBeenCalledWith('pause-2', 'pause');
  });
});

it('permanently deletes every selected completed task from the multi-selection menu', async () => {
  const first: QueueTask = { ...oldTask, id: 'done-delete-1', name: 'first-done.zip', status: 'completed', progress: 100 };
  const second: QueueTask = { ...oldTask, id: 'done-delete-2', name: 'second-done.zip', status: 'completed', progress: 100 };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'terminal' ? [first, second] : []));
  state.deleteTasks.mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<App />);

  fireEvent.click(await screen.findByRole('tab', { name: /已完成/ }));
  fireEvent.click(await screen.findByText('first-done.zip'));
  fireEvent.click(screen.getByText('second-done.zip'), { ctrlKey: true });
  fireEvent.contextMenu(screen.getByText('first-done.zip'));
  await user.click(screen.getByRole('menuitem', { name: '删除' }));
  const dialog = screen.getByRole('dialog', { name: '彻底删除任务' });
  const deleteFiles = within(dialog).getByRole('checkbox', { name: '同时删除下载文件' });
  if ((deleteFiles as HTMLInputElement).checked) await user.click(deleteFiles);
  await user.click(within(dialog).getByRole('button', { name: '彻底删除任务' }));

  await waitFor(() => expect(state.deleteTasks).toHaveBeenCalledTimes(1));
  expect(new Set(state.deleteTasks.mock.calls[0]?.[0])).toEqual(new Set(['done-delete-1', 'done-delete-2']));
  expect(state.deleteTasks.mock.calls[0]?.[1]).toBe(false);
});

it('clears cleanup retry only after the typed task action result reports success', async () => {
  const cleanupTask: QueueTask = {
    id: 'cleanup-task', name: 'cleanup.bin', kind: 'cloud', status: 'completed', progress: 100,
    source: 'cloud-share://baidu/share/entry', cleanup_pending: true, error: 'opaque provider failure',
  };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [] : [cleanupTask]));
  state.taskAction.mockResolvedValueOnce({
    task: { ...cleanupTask, cleanup_pending: false, error: undefined },
  });
  render(<App />);
  fireEvent.click(await screen.findByRole('tab', { name: /已完成/ }));

  fireEvent.click(await screen.findByRole('button', { name: '重试网盘清理 cleanup.bin' }));

  await waitFor(() => expect(state.taskAction).toHaveBeenCalledWith('cleanup-task', 'retry_cleanup'));
  await waitFor(() => expect(screen.queryByRole('button', { name: '重试网盘清理 cleanup.bin' })).not.toBeInTheDocument());
});

it('keeps typed cleanup retry available when the task action fails', async () => {
  const cleanupTask: QueueTask = {
    id: 'cleanup-task', name: 'cleanup.bin', kind: 'cloud', status: 'completed', progress: 100,
    source: 'cloud-share://baidu/share/entry', cleanup_pending: true,
  };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [] : [cleanupTask]));
  state.taskAction.mockRejectedValueOnce(new Error('cleanup retry failed'));
  render(<App />);
  fireEvent.click(await screen.findByRole('tab', { name: /已完成/ }));

  fireEvent.click(await screen.findByRole('button', { name: '重试网盘清理 cleanup.bin' }));

  expect(await screen.findByRole('status')).toHaveTextContent('cleanup retry failed');
  expect(screen.getByRole('button', { name: '重试网盘清理 cleanup.bin' })).toBeVisible();
});

it('upserts a parsed task that already arrived through a live event', async () => {
  state.list.mockResolvedValueOnce([]);
  state.parse.mockResolvedValueOnce({ task: liveTask });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  act(() => { state.onTask?.(liveTask); });
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://example.test/archive.iso' }); });

  expect(within(screen.getByRole('region', { name: '下载队列' })).getAllByText('archive.iso')).toHaveLength(1);
});

it('keeps a protected share open for an access code before completing its normal parse result', async () => {
  const user = userEvent.setup();
  const shareLink = 'https://pan.baidu.com/s/protected-share';
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([{ input: shareLink, valid: true, kind: 'cloud_share' }]);
  state.parse
    .mockResolvedValueOnce({ shareAccessCodeRequired: { provider: 'baidu', message: '请输入提取码' } })
    .mockResolvedValueOnce({ task: liveTask });
  render(<App />);

  await user.type(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), `${shareLink}{Enter}`);
  const accessCode = await screen.findByLabelText(`${shareLink}访问码`);
  expect(screen.getByRole('dialog', { name: '新建下载' })).toBeVisible();
  expect(state.parse).toHaveBeenLastCalledWith(shareLink, undefined, true);

  await user.type(accessCode, '1234');
  await user.click(screen.getByRole('button', { name: '重新解析' }));

  await waitFor(() => expect(state.parse).toHaveBeenLastCalledWith(shareLink, '1234', true));
  await user.click(await screen.findByRole('button', { name: '新建任务' }));
  await waitFor(() => expect(screen.queryByRole('dialog', { name: '新建下载' })).not.toBeInTheDocument());
  expect(screen.getByText('archive.iso')).toBeVisible();
});

it('uses a non-link query to find local active, completed, and failed tasks', async () => {
  const user = userEvent.setup();
  const active = { id: 'active-document', name: '正在下载的文档.zip', kind: 'file' as const, status: 'downloading' as const, progress: 40 };
  const completed = { id: 'completed-document', name: '已完成文档.pdf', kind: 'file' as const, status: 'completed' as const, progress: 100 };
  const failed = { id: 'failed-document', name: '失败的文档.docx', kind: 'file' as const, status: 'failed' as const, progress: 0 };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [active] : scope === 'terminal' ? [completed, failed] : [active, completed, failed]));
  render(<App />);

  await user.type(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), '文档');

  const results = await screen.findByRole('region', { name: '搜索结果' });
  expect(within(results).getByRole('button', { name: /正在下载的文档\.zip/ })).toBeVisible();
  expect(within(results).getByRole('button', { name: /已完成文档\.pdf/ })).toBeVisible();
  expect(within(results).getByRole('button', { name: /失败的文档\.docx/ })).toBeVisible();

  await user.click(within(results).getByRole('button', { name: /已完成文档\.pdf/ }));
  expect(screen.getByRole('tab', { name: /已完成/ })).toHaveAttribute('aria-selected', 'true');
  expect(screen.getByTitle('已完成文档.pdf')).toBeVisible();
});

it('clears a task search from the search input and closes its attached results panel', async () => {
  const user = userEvent.setup();
  const task = { id: 'search-document', name: '搜索文档.zip', kind: 'file' as const, status: 'downloading' as const, progress: 40 };
  state.list.mockResolvedValue([task]);
  render(<App />);

  const input = screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' });
  await user.type(input, '搜索{Enter}');
  await screen.findByRole('region', { name: '搜索结果' });

  const search = input.closest('.global-search')!;
  expect(search).toHaveClass('has-search-results');
  const clear = within(search).getByRole('button', { name: '清除搜索' });
  await user.click(clear);

  expect(input).toHaveValue('');
  expect(screen.queryByRole('region', { name: '搜索结果' })).not.toBeInTheDocument();
});

it('closes live search results with Escape without clearing the query', async () => {
  const user = userEvent.setup();
  const task = { id: 'escape-document', name: '保留关键词文档.zip', kind: 'file' as const, status: 'downloading' as const, progress: 40 };
  state.list.mockResolvedValue([task]);
  render(<App />);

  const input = screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' });
  await user.type(input, '关键词');
  await screen.findByRole('region', { name: '搜索结果' });
  await user.keyboard('{Escape}');

  expect(input).toHaveValue('关键词');
  expect(screen.queryByRole('region', { name: '搜索结果' })).not.toBeInTheDocument();
});

it('moves through live search results with arrow keys and opens the selected task with Enter', async () => {
  const user = userEvent.setup();
  const active = { id: 'keyboard-active', name: '键盘下载中.zip', kind: 'file' as const, status: 'downloading' as const, progress: 40 };
  const completed = { id: 'keyboard-completed', name: '键盘已完成.zip', kind: 'file' as const, status: 'completed' as const, progress: 100 };
  state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active' ? [active] : scope === 'terminal' ? [completed] : [active, completed]));
  render(<App />);

  const input = screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' });
  await user.type(input, '键盘');
  const results = await screen.findByRole('region', { name: '搜索结果' });
  const activeResult = within(results).getByRole('button', { name: /键盘下载中\.zip/ });
  const completedResult = within(results).getByRole('button', { name: /键盘已完成\.zip/ });
  const [firstResult, secondResult] = within(results).getAllByRole('button');
  const selectedTask = secondResult === activeResult ? active : completed;

  expect(input).toHaveFocus();
  await user.keyboard('{ArrowDown}');
  expect(firstResult).toHaveClass('is-keyboard-selected');
  await user.keyboard('{ArrowDown}');
  expect(secondResult).toHaveClass('is-keyboard-selected');
  await user.keyboard('{ArrowUp}');
  expect(firstResult).toHaveClass('is-keyboard-selected');
  await user.keyboard('{ArrowDown}{Enter}');

  expect(screen.getByRole('tab', { name: selectedTask.status === 'completed' ? /已完成/ : /下载中/ })).toHaveAttribute('aria-selected', 'true');
  expect(screen.getByTitle(selectedTask.name)).toBeVisible();
});

it('opens the unified batch window immediately when pasted content spans multiple lines', async () => {
  render(<App />);
  const content = 'https://example.test/first.zip\nhttps://example.test/second.zip';
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([
    { input: 'https://example.test/first.zip', valid: true, kind: 'direct' },
    { input: 'https://example.test/second.zip', valid: true, kind: 'direct' },
  ]);
  state.parse.mockImplementation(async (input: string) => ({ confirmation: {
    taskId: input, title: input.split('/').pop(), requiresConfirmation: true, resources: [], processing: [],
  } }));

  fireEvent.paste(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), {
    clipboardData: { getData: () => content }
  });

  const dialog = await screen.findByRole('dialog', { name: '新建下载' });
  expect(dialog).toHaveClass('batch-download-dialog', 'modal');
  expect(await within(dialog).findByText('first.zip')).toBeVisible();
  expect(within(dialog).getByText('second.zip')).toBeVisible();
});

it('opens one batch dialog for pasted links and closes after the first selected task is created', async () => {
  const user = userEvent.setup();
  const firstLink = 'https://example.test/first.zip';
  const secondLink = 'https://example.test/second.zip';
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([
    { input: firstLink, valid: true, kind: 'direct' },
    { input: secondLink, valid: true, kind: 'direct' },
  ]);
  state.parse.mockImplementation(async (input: string) => ({
    confirmation: {
      taskId: input === firstLink ? 'first-draft' : 'second-draft',
      title: input === firstLink ? 'first.zip' : 'second.zip',
      requiresConfirmation: true,
      resources: [],
      processing: [],
    },
  }));
  const first = deferred<QueueTask | undefined>();
  const second = deferred<QueueTask | undefined>();
  state.confirm.mockImplementation((taskId: string) => taskId === 'first-draft' ? first.promise : second.promise);
  render(<App />);

  fireEvent.paste(screen.getByRole('textbox', { name: '搜索任务或粘贴下载链接' }), {
    clipboardData: { getData: () => `${firstLink}\n${secondLink}` },
  });

  const dialog = await screen.findByRole('dialog', { name: '新建下载' });
  expect(within(dialog).getByText('first.zip')).toBeVisible();
  expect(within(dialog).getByText('second.zip')).toBeVisible();
  await user.click(within(dialog).getByRole('button', { name: '后处理与密码' }));
  await user.type(within(dialog).getByRole('textbox', { name: '后处理与密码' }), '密码 88in');
  await user.click(within(dialog).getByRole('button', { name: '新建任务' }));

  await waitFor(() => expect(state.confirm).toHaveBeenCalledTimes(2));
  expect(state.confirm).toHaveBeenCalledWith('first-draft', [], undefined, false, '密码 88in');
  expect(state.confirm).toHaveBeenCalledWith('second-draft', [], undefined, false, '密码 88in');
  await act(async () => { first.resolve({ id: 'first-task', name: 'first.zip', kind: 'file', status: 'downloading', progress: 0 }); });
  await waitFor(() => expect(screen.queryByRole('dialog', { name: '新建下载' })).not.toBeInTheDocument());
  expect(screen.getByRole('tab', { name: /下载中/ })).toHaveAttribute('aria-selected', 'true');
  expect(screen.getByTitle('first.zip')).toBeVisible();
  await act(async () => { second.resolve({ id: 'second-task', name: 'second.zip', kind: 'file', status: 'queued', progress: 0 }); });
});

it('queues normal share results and presents duplicate results in FIFO order', async () => {
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://baidu/plan', title: '分享目录', requiresConfirmation: true,
      resources: [{ id: 'f1', name: 'first.bin', size: '1 B', selected: true }], processing: []
    }
  });
  state.confirmCloud.mockResolvedValueOnce({ outcomes: [
    { task: { id: 'queued-1', name: 'first.bin', kind: 'cloud', status: 'downloading', progress: 0 } },
    { duplicate: { task: { id: 'duplicate-1', name: 'first-existing.bin', kind: 'cloud', status: 'completed', progress: 100 }, state: 'completed_files_present' } },
    { task: { id: 'queued-2', name: 'second.bin', kind: 'cloud', status: 'downloading', progress: 0 } },
    { duplicate: { task: { id: 'duplicate-2', name: 'second-existing.bin', kind: 'cloud', status: 'downloading', progress: 20 }, state: 'existing_active' } }
  ], failures: [] });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://pan.baidu.com/s/share' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));

  const firstDuplicateDialog = await screen.findByRole('dialog', { name: '检测到重复下载' });
  expect(firstDuplicateDialog).toHaveTextContent('first-existing.bin');
  expect(within(screen.getByRole('region', { name: '下载队列' })).getByTitle('first.bin')).toBeVisible();
  expect(within(screen.getByRole('region', { name: '下载队列' })).getByTitle('second.bin')).toBeVisible();
  expect(firstDuplicateDialog).not.toHaveTextContent('second-existing.bin');

  const cancel = screen.getByRole('button', { name: '取消' });
  fireEvent.click(cancel);
  fireEvent.click(cancel);
  await waitFor(() => expect(screen.getByRole('dialog', { name: '检测到重复下载' })).toHaveTextContent('second-existing.bin'));
});

it('closes and selects the named share root as soon as creation is acknowledged, before the batch completes', async () => {
  const submission = deferred<{ outcomes: never[]; failures: never[] }>();
  state.parse.mockResolvedValueOnce({ confirmation: {
    taskId: 'cloud-share://pikpak/early-plan', title: '原始分享目录', requiresConfirmation: true,
    resources: [{ id: 'f1', name: 'first.mp4', size: '1 MB', selected: true }], processing: [],
  } });
  state.confirmCloud.mockReturnValueOnce(submission.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  fireEvent.click(screen.getByRole('tab', { name: /已完成/ }));
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://mypikpak.com/s/early-plan' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));
  expect(screen.getByRole('dialog', { name: '确认下载内容' })).toBeVisible();
  const created = state.confirmCloud.mock.calls[0][4] as ((task: QueueTask) => void) | undefined;
  await act(async () => { created?.({ id: 'early-root', name: '原始分享目录', kind: 'folder', status: 'queued', progress: 0 }); });
  expect(screen.queryByRole('dialog', { name: '确认下载内容' })).not.toBeInTheDocument();
  const row = within(screen.getByRole('region', { name: '下载队列' })).getByTitle('原始分享目录').closest('article');
  expect(row).toHaveAttribute('aria-selected', 'true');
  await act(async () => { submission.resolve({ outcomes: [], failures: [] }); });
});

it('shows the duplicate decision when downloading an existing Baidu file with a queue projection', async () => {
  const failed: QueueTask = { id: 'existing-baidu', name: 'book.pdf', kind: 'cloud', status: 'failed', progress: 0 };
  state.cloudInvoke.mockImplementation(async (command: string) => {
    if (command === 'cloud_provider_catalog') return cloudCatalog.filter(d => d.provider === 'baidu');
    if (command === 'cloud_account') return { provider: 'baidu', id: '7', displayName: 'Baidu', authMethod: 'web' };
    if (command === 'cloud_list_directory') return { id: '/', items: [{ id: 'file-7', parentId: '/', name: 'book.pdf', kind: 'file', size: 123 }] };
    if (command === 'cloud_saved_credentials') return null;
    if (command === 'cloud_download_url') return { ...failed, duplicate: 'overwrite_required', queueTask: failed };
    throw new Error(`Unexpected command: ${command}`);
  });
  const user = userEvent.setup();
  render(<App />);
  await user.click(await screen.findByRole('button', { name: '我的网盘' }));
  await user.click(await screen.findByRole('button', { name: '百度网盘' }));
  await user.click(await screen.findByRole('checkbox', { name: '选择 book.pdf' }));
  await user.click(screen.getByRole('button', { name: '下载', exact: true }));
  expect(await screen.findByRole('dialog', { name: '检测到重复下载' })).toHaveTextContent('book.pdf');
});

it('uses the single directory root returned for a confirmed share folder', async () => {
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://quark/share-plan', title: '分享目录', requiresConfirmation: true,
      resources: [
        { id: 'f1', name: 'first.jpg', size: '1 B', selected: true },
        { id: 'f2', name: 'nested/second.jpg', size: '2 B', selected: true },
      ], processing: []
    }
  });
  state.confirmCloud.mockResolvedValueOnce({
    outcomes: [{
      task: { id: 'share-leaf', name: 'first.jpg', kind: 'cloud', status: 'downloading', progress: 0 },
      queueTask: {
        id: 'share-directory-root', name: '夸克分享目录', kind: 'folder', status: 'downloading', progress: 0,
        children: [
          { id: 'share-leaf', name: 'first.jpg', kind: 'cloud', status: 'downloading', progress: 0 },
          { id: 'nested', name: 'nested', kind: 'folder', status: 'downloading', progress: 0, children: [] },
        ],
      },
    }],
    failures: [],
  });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://pan.quark.cn/s/share-plan' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 2/ }));

  await waitFor(() => expect(screen.queryByRole('dialog', { name: '确认下载内容' })).not.toBeInTheDocument());
  const queue = screen.getByRole('region', { name: '下载队列' });
  expect(within(queue).getByTitle('夸克分享目录')).toBeVisible();
  expect(queue.querySelectorAll('article.task-row')).toHaveLength(1);
});

it('restores only failed selections for retry after early task creation closed the dialog', async () => {
  const submission = deferred<{ outcomes: never[]; failures: { selectedId: string; code: string; message: string }[] }>();
  state.parse.mockResolvedValueOnce({ confirmation: {
    taskId: 'cloud-share://pikpak/partial-created', title: '资料', requiresConfirmation: true,
    resources: [{ id: 'ok', name: 'ok.mp4', size: '1 B', selected: true }, { id: 'bad', name: 'bad.mp4', size: '2 B', selected: true }], processing: [],
  } });
  state.confirmCloud.mockReturnValueOnce(submission.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://mypikpak.com/s/partial-created' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 2/ }));
  await act(async () => { state.confirmCloud.mock.calls[0][4]({ id: 'ok-task', name: 'ok.mp4', kind: 'cloud', status: 'parsing', progress: 0 }); });
  expect(screen.queryByRole('dialog', { name: '确认下载内容' })).not.toBeInTheDocument();
  await act(async () => { submission.resolve({ outcomes: [], failures: [{ selectedId: 'bad', code: 'network_error', message: '网络暂时不可用' }] }); });
  expect(screen.getByRole('dialog', { name: '确认下载内容' })).toBeVisible();
  expect(screen.getByRole('checkbox', { name: '选择 ok.mp4' })).not.toBeChecked();
  expect(screen.getByRole('checkbox', { name: '选择 bad.mp4' })).toBeChecked();
  expect(screen.getByRole('button', { name: /立即下载 1/ })).toBeEnabled();
  expect(screen.getByRole('status')).toHaveTextContent('网络暂时不可用');
});

it('keeps only failed share entries selected while preserving earlier successful outcomes', async () => {
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://uc/partial-plan', title: 'UC 分享', requiresConfirmation: true,
      resources: [
        { id: 'ok', name: 'ok.jpg', size: '1 B', selected: true },
        { id: 'failed', name: 'failed.jpg', size: '2 B', selected: true },
      ], processing: []
    }
  });
  state.confirmCloud.mockResolvedValueOnce({
    outcomes: [{ task: { id: 'queued-ok', name: 'ok.jpg', kind: 'cloud', status: 'downloading', progress: 0 } }],
    failures: [{ selectedId: 'failed', code: 'web_auth_required', message: '网盘需要网页验证' }],
  });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/partial-plan' }); });

  fireEvent.click(screen.getByRole('button', { name: /立即下载 2/ }));

  expect(await screen.findByRole('status')).toHaveTextContent('网盘需要网页验证');
  expect(within(screen.getByRole('region', { name: '下载队列' })).getByTitle('ok.jpg')).toBeVisible();
  expect(screen.getByRole('dialog', { name: '确认下载内容' })).toBeVisible();
  expect(screen.getByRole('checkbox', { name: '选择 ok.jpg' })).not.toBeChecked();
  expect(screen.getByRole('checkbox', { name: '选择 failed.jpg' })).toBeChecked();
});

it('keeps share confirmation open after one rejected submission and ignores concurrent clicks', async () => {
  const submission = deferred<never>();
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://uc/plan', title: 'UC 分享', requiresConfirmation: true,
      resources: [{ id: 'f1', name: 'photo.jpg', size: '1 B', selected: true }], processing: []
    }
  });
  state.confirmCloud.mockReturnValueOnce(submission.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/share' }); });

  const submit = screen.getByRole('button', { name: /立即下载 1/ });
  fireEvent.click(submit);
  fireEvent.click(submit);
  expect(state.confirmCloud).toHaveBeenCalledTimes(1);

  await act(async () => { submission.reject(new Error('服务器拒绝下载')); });
  expect(screen.getByRole('dialog', { name: '确认下载内容' })).toBeVisible();
  expect(await screen.findByRole('status')).toHaveTextContent('服务器拒绝下载');
});

it('keeps share confirmation open and creates nothing when transfer capacity preflight rejects', async () => {
  state.parse.mockResolvedValueOnce({ confirmation: {
    taskId: 'cloud-share://aliyun/plan', title: '阿里云盘分享', requiresConfirmation: true,
    resources: [{ id: 'f1', name: 'movie.mp4', size: '92.5 MB', selected: true }], processing: [],
  } });
  state.confirmCloud.mockRejectedValueOnce(new Error(
    '阿里云盘转存空间不足：本次需要 92.5 MB，当前可用 10.0 MB，未建立下载任务。'
  ));
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => {
    state.listeners.get('launcher-parse')?.({ payload: 'https://www.alipan.com/s/share' });
  });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));
  expect(await screen.findByRole('status')).toHaveTextContent('未建立下载任务');
  expect(screen.getByRole('dialog', { name: '确认下载内容' })).toBeVisible();
  expect(state.confirmCloud.mock.calls[0][4]).toBeTypeOf('function');
  expect(within(screen.getByRole('region', { name: '下载队列' })).queryByTitle('movie.mp4')).not.toBeInTheDocument();
});

it('binds in-flight share confirmation to its plan without blocking or clearing a newer plan', async () => {
  const firstSubmission = deferred<{ outcomes: never[]; failures: never[] }>();
  const secondSubmission = deferred<{ outcomes: never[]; failures: never[] }>();
  state.parse
    .mockResolvedValueOnce({
      confirmation: {
        taskId: 'cloud-share://uc/plan-a', title: '分享 A', requiresConfirmation: true,
        resources: [{ id: 'a1', name: 'first.jpg', size: '1 B', selected: true }], processing: []
      }
    })
    .mockResolvedValueOnce({
      confirmation: {
        taskId: 'cloud-share://uc/plan-b', title: '分享 B', requiresConfirmation: true,
        resources: [{ id: 'b1', name: 'second.jpg', size: '1 B', selected: true }], processing: []
      }
    });
  state.confirmCloud
    .mockReturnValueOnce(firstSubmission.promise)
    .mockReturnValueOnce(secondSubmission.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/plan-a' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));
  fireEvent.click(screen.getByRole('button', { name: '关闭确认窗口' }));
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/plan-b' }); });
  expect(screen.getByRole('checkbox', { name: '选择 second.jpg' })).toBeChecked();
  expect(screen.queryByRole('checkbox', { name: '选择 first.jpg' })).not.toBeInTheDocument();

  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));
  expect(state.confirmCloud).toHaveBeenCalledTimes(2);

  await act(async () => { firstSubmission.resolve({ outcomes: [], failures: [] }); });
  expect(screen.getByRole('checkbox', { name: '选择 second.jpg' })).toBeChecked();
  expect(screen.queryByRole('checkbox', { name: '选择 first.jpg' })).not.toBeInTheDocument();

  await act(async () => { secondSubmission.reject(new Error('B 下载失败')); });
  expect(screen.getByRole('checkbox', { name: '选择 second.jpg' })).toBeChecked();
  expect(screen.queryByRole('checkbox', { name: '选择 first.jpg' })).not.toBeInTheDocument();
});

it('does not show a rejected response from a closed share generation over a newer confirmation', async () => {
  const firstSubmission = deferred<{ outcomes: never[]; failures: never[] }>();
  state.parse
    .mockResolvedValueOnce({
      confirmation: {
        taskId: 'cloud-share://uc/plan-a', title: '分享 A', requiresConfirmation: true,
        resources: [{ id: 'a1', name: 'first.jpg', size: '1 B', selected: true }], processing: []
      }
    })
    .mockResolvedValueOnce({
      confirmation: {
        taskId: 'cloud-share://uc/plan-b', title: '分享 B', requiresConfirmation: true,
        resources: [{ id: 'b1', name: 'second.jpg', size: '1 B', selected: true }], processing: []
      }
    });
  state.confirmCloud.mockReturnValueOnce(firstSubmission.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/plan-a' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));
  fireEvent.click(screen.getByRole('button', { name: '关闭确认窗口' }));
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/plan-b' }); });

  await act(async () => { firstSubmission.reject(new Error('A 已过期错误')); });

  expect(screen.getByRole('checkbox', { name: '选择 second.jpg' })).toBeChecked();
  expect(screen.queryByRole('checkbox', { name: '选择 first.jpg' })).not.toBeInTheDocument();
  expect(screen.queryByText('A 已过期错误')).not.toBeInTheDocument();
});

it('does not show a rejected response after its share confirmation is closed', async () => {
  const submission = deferred<{ outcomes: never[]; failures: never[] }>();
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://uc/closed-plan', title: '已关闭分享', requiresConfirmation: true,
      resources: [{ id: 'f1', name: 'closed.jpg', size: '1 B', selected: true }], processing: []
    }
  });
  state.confirmCloud.mockReturnValueOnce(submission.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/closed-plan' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));
  fireEvent.click(screen.getByRole('button', { name: '关闭确认窗口' }));
  await act(async () => { submission.reject(new Error('关闭后过期错误')); });

  expect(screen.queryByRole('dialog', { name: '确认下载内容' })).not.toBeInTheDocument();
  expect(screen.queryByText('关闭后过期错误')).not.toBeInTheDocument();
});

it('does not let an old response clear a reparsed confirmation for the same share plan', async () => {
  const firstSubmission = deferred<{ outcomes: never[]; failures: never[] }>();
  const secondSubmission = deferred<{ outcomes: never[]; failures: never[] }>();
  const confirmation = {
    taskId: 'cloud-share://uc/same-plan', title: '同一分享', requiresConfirmation: true,
    resources: [
      { id: 'f1', name: 'first.jpg', size: '1 B', selected: true },
      { id: 'f2', name: 'second.jpg', size: '1 B', selected: true },
    ],
    processing: []
  };
  state.parse
    .mockResolvedValueOnce({ confirmation })
    .mockResolvedValueOnce({ confirmation });
  state.confirmCloud
    .mockReturnValueOnce(firstSubmission.promise)
    .mockReturnValueOnce(secondSubmission.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/same-plan' }); });
  fireEvent.click(screen.getByRole('checkbox', { name: '选择 second.jpg' }));
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));
  fireEvent.click(screen.getByRole('button', { name: '关闭确认窗口' }));

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://drive.uc.cn/s/same-plan' }); });
  fireEvent.click(screen.getByRole('checkbox', { name: '选择 first.jpg' }));
  expect(screen.getByRole('checkbox', { name: '选择 second.jpg' })).toBeChecked();
  const secondSubmit = screen.getByRole('button', { name: /立即下载 1/ });
  fireEvent.click(secondSubmit);
  fireEvent.click(secondSubmit);
  expect(state.confirmCloud).toHaveBeenCalledTimes(2);
  expect(state.confirmCloud).toHaveBeenLastCalledWith('cloud-share://uc/same-plan', ['f2'], undefined, false, expect.any(Function));

  await act(async () => { firstSubmission.resolve({ outcomes: [], failures: [] }); });

  expect(screen.getByRole('dialog', { name: '确认下载内容' })).toBeVisible();
  expect(screen.getByRole('checkbox', { name: '选择 first.jpg' })).not.toBeChecked();
  expect(screen.getByRole('checkbox', { name: '选择 second.jpg' })).toBeChecked();
  await act(async () => { secondSubmission.resolve({ outcomes: [], failures: [] }); });
  expect(screen.queryByRole('dialog', { name: '确认下载内容' })).not.toBeInTheDocument();
});

it('ignores concurrent duplicate resolutions and advances to the next duplicate after success', async () => {
  const resolution = deferred<QueueTask>();
  state.parse.mockResolvedValueOnce({
    confirmation: {
      taskId: 'cloud-share://baidu/plan', title: '分享目录', requiresConfirmation: true,
      resources: [{ id: 'f1', name: 'first.bin', size: '1 B', selected: true }], processing: []
    }
  });
  state.confirmCloud.mockResolvedValueOnce({ outcomes: [
    { duplicate: { task: { id: 'duplicate-1', name: 'first-existing.bin', kind: 'cloud', status: 'paused', progress: 40 }, state: 'resume_available' } },
    { duplicate: { task: { id: 'duplicate-2', name: 'second-existing.bin', kind: 'cloud', status: 'downloading', progress: 20 }, state: 'existing_active' } }
  ], failures: [] });
  state.resolveDuplicate.mockReturnValueOnce(resolution.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://pan.baidu.com/s/share' }); });
  fireEvent.click(screen.getByRole('button', { name: /立即下载 1/ }));

  const resume = await screen.findByRole('button', { name: '继续下载' });
  fireEvent.click(resume);
  fireEvent.click(resume);
  expect(state.resolveDuplicate).toHaveBeenCalledTimes(1);

  await act(async () => { resolution.resolve({ id: 'duplicate-1', name: 'first-existing.bin', kind: 'cloud', status: 'downloading', progress: 40 }); });
  await waitFor(() => expect(screen.getByRole('dialog', { name: '检测到重复下载' })).toHaveTextContent('second-existing.bin'));
});

it('closes an overwrite confirmation before its native resolution completes', async () => {
  const resolution = deferred<QueueTask>();
  state.parse.mockResolvedValueOnce({
    duplicate: {
      task: { id: 'overwrite-existing', name: 'overwrite.bin', kind: 'file', status: 'failed', progress: 28 },
      state: 'overwrite_required',
    },
  });
  state.resolveDuplicate.mockReturnValueOnce(resolution.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());

  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'https://example.test/overwrite.bin' }); });
  const dialog = await screen.findByRole('dialog', { name: '检测到重复下载' });
  fireEvent.click(within(dialog).getByRole('button', { name: '覆盖并重新下载' }));

  expect(state.resolveDuplicate).toHaveBeenCalledWith('overwrite-existing', 'overwrite');
  expect(screen.queryByRole('dialog', { name: '检测到重复下载' })).not.toBeInTheDocument();

  await act(async () => { resolution.resolve({ id: 'overwrite-existing', name: 'overwrite.bin', kind: 'file', status: 'downloading', progress: 0 }); });
});

it('retrieves a compatible service model from its selector and persists the selected AI configuration', async () => {
  const user = userEvent.setup();
  state.listAiModels.mockResolvedValue(['gpt-4.1-mini', 'gpt-5-mini']);
  state.updateAiSettings.mockResolvedValue({ enabled: true, baseUrl: 'https://gateway.example.test/v1', model: 'gpt-5-mini', apiKeyConfigured: true });
  render(<App />);

  await user.click(screen.getByRole('button', { name: /设置/ }));
  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.click(screen.getByRole('checkbox', { name: '启用 AI 智能分析' }));
  await user.type(screen.getByRole('textbox', { name: '服务地址' }), 'https://gateway.example.test/v1');
  await user.type(screen.getByLabelText('API Key'), 'sk-local-secret');
  fireEvent.pointerDown(screen.getByRole('combobox', { name: '模型' }));
  await screen.findByRole('option', { name: 'gpt-5-mini' });
  await user.selectOptions(screen.getByRole('combobox', { name: '模型' }), 'gpt-5-mini');

  await waitFor(() => expect(state.updateAiSettings).toHaveBeenCalledWith({
    enabled: true,
    baseUrl: 'https://gateway.example.test/v1',
    model: 'gpt-5-mini',
    apiKey: 'sk-local-secret'
  }));
});

it('opens the normal new-download dialog from the floating-ball menu command', async () => {
  state.list.mockResolvedValueOnce([]);
  state.clipboardRead.mockResolvedValue('notes without a link');
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-command')).toBeDefined());

  act(() => { state.listeners.get('launcher-command')?.({ payload: 'new-download' }); });

  const dialog = await screen.findByRole('dialog', { name: t('download.new') });
  expect(within(dialog).getByRole('textbox', { name: t('download.content') })).toHaveValue('');
  expect(state.clipboardRead).toHaveBeenCalledOnce();
});

it('parses a clipboard link from the floating-ball menu and waits for task confirmation', async () => {
  const clipboard = '分享地址：https://example.test/archive.zip';
  const link = 'https://example.test/archive.zip';
  state.clipboardRead.mockResolvedValue(clipboard);
  vi.mocked(scanDownloadInputs).mockResolvedValueOnce([{ input: link, valid: true, kind: 'direct' }]);
  state.parse.mockResolvedValueOnce({ confirmation: {
    taskId: 'clipboard-draft', title: 'archive.zip', requiresConfirmation: true,
    resources: [], processing: [],
  } });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-command')).toBeDefined());

  act(() => { state.listeners.get('launcher-command')?.({ payload: 'new-download' }); });

  const dialog = await screen.findByRole('dialog', { name: t('download.new') });
  expect(await within(dialog).findByText('archive.zip')).toBeVisible();
  expect(scanDownloadInputs).toHaveBeenCalledWith(clipboard);
  expect(state.parse).toHaveBeenCalledWith(link, undefined, true);
  expect(within(dialog).getByRole('button', { name: t('download.newTask') })).toBeVisible();
  expect(state.confirm).not.toHaveBeenCalled();
});

it('allows manual entry when the floating-ball menu cannot read the clipboard', async () => {
  state.clipboardRead.mockRejectedValue(new Error('clipboard unavailable'));
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-command')).toBeDefined());

  act(() => { state.listeners.get('launcher-command')?.({ payload: 'new-download' }); });

  const dialog = await screen.findByRole('dialog', { name: t('download.new') });
  expect(within(dialog).getByRole('textbox', { name: t('download.content') })).toHaveValue('');
  expect(state.clipboardRead).toHaveBeenCalledOnce();
});

it('opens the normal new-download dialog with an accepted floating-ball drop', async () => {
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-new-download')).toBeDefined());

  act(() => {
    state.listeners.get('launcher-new-download')?.({
      payload: 'magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567',
    });
  });

  expect(screen.getByRole('dialog', { name: t('download.new') })).toBeVisible();
  await waitFor(() => expect(scanDownloadInputs).toHaveBeenCalledWith('magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567'));
});

it('opens a new task with the links that launched the app from a browser', async () => {
  const links = ['magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567', 'ed2k://|file|a.mkv|1024|0123456789ABCDEF0123456789ABCDEF|/'];
  state.takeExternalLinks.mockResolvedValueOnce(links);
  render(<App />);

  expect(await screen.findByRole('dialog', { name: t('download.new') })).toBeVisible();
  await waitFor(() => expect(scanDownloadInputs).toHaveBeenCalledWith(links.join('\n')));
  expect(state.confirm).not.toHaveBeenCalled();
});

it('opens a new task when a browser hands a link to the running app', async () => {
  const link = 'thunder://QUFodHRwOi8vZXhhbXBsZS5jb20vYS56aXBaWg==';
  render(<App />);
  await waitFor(() => expect(state.listeners.get('external-links-available')).toBeDefined());
  expect(screen.queryByRole('dialog', { name: t('download.new') })).toBeNull();

  state.takeExternalLinks.mockResolvedValueOnce([link]);
  act(() => { state.listeners.get('external-links-available')?.({ payload: null }); });

  expect(await screen.findByRole('dialog', { name: t('download.new') })).toBeVisible();
  await waitFor(() => expect(scanDownloadInputs).toHaveBeenCalledWith(link));
});

it('switches the manager to all tasks when the launcher is double-clicked', async () => {
  const user = userEvent.setup();
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-open-all-tasks')).toBeDefined());

  await user.click(screen.getByRole('tab', { name: /已完成/ }));
  expect(screen.getByRole('tab', { name: /已完成/ })).toHaveAttribute('aria-selected', 'true');
  act(() => { state.listeners.get('launcher-open-all-tasks')?.({ payload: undefined }); });

  expect(screen.getByRole('tab', { name: /全部/ })).toHaveAttribute('aria-selected', 'true');
});

it('expands primary navigation when the launcher opens a non-download workspace', async () => {
  const user = userEvent.setup();
  render(<App />);
  const navigation = screen.getByRole('navigation', { name: '主导航' });
  await waitFor(() => expect(state.listeners.get('launcher-navigate')).toBeDefined());

  await user.click(screen.getByRole('button', { name: '收起侧边栏' }));
  expect(navigation).toHaveAttribute('aria-hidden', 'true');

  act(() => { state.listeners.get('launcher-navigate')?.({ payload: 'cloud' }); });

  expect(navigation).toHaveAttribute('aria-hidden', 'false');
  expect(screen.getByRole('region', { name: '网盘管理' })).toBeVisible();
});

it('uses the existing native task action for floating-ball batch resume', async () => {
  const pausedTask: QueueTask = { ...oldTask, id: 'paused-task', status: 'paused' };
  state.list.mockResolvedValueOnce([pausedTask]);
  const action = deferred<{ task?: QueueTask }>();
  state.taskAction.mockReturnValueOnce(action.promise);
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-command')).toBeDefined());
  await screen.findByText('archive.iso');

  act(() => { state.listeners.get('launcher-command')?.({ payload: 'resume-all' }); });

  await waitFor(() => expect(state.taskAction).toHaveBeenCalledWith('paused-task', 'resume'));
  expect(screen.getByRole('button', { name: 'archive.iso 排队中' })).toBeEnabled();
  await act(async () => { action.resolve({ task: { ...pausedTask, status: 'downloading' } }); });
  expect(await screen.findByRole('button', { name: '暂停 archive.iso' })).toBeVisible();
});

it('pauses queued tasks before freeing active slots when the launcher pauses all', async () => {
  const active: QueueTask = { ...oldTask, id: 'active' };
  const queued: QueueTask = { ...oldTask, id: 'queued', name: 'waiting.zip', status: 'queued' };
  state.list.mockResolvedValue([active, queued]);
  const waitingPause = deferred<{ task: QueueTask }>();
  state.taskAction.mockImplementation((id: string) => id === queued.id
    ? waitingPause.promise : Promise.resolve({ task: { ...active, status: 'paused' } }));
  render(<App />);
  await screen.findByText('waiting.zip');
  await waitFor(() => expect(state.listeners.get('launcher-command')).toBeDefined());
  act(() => { state.listeners.get('launcher-command')?.({ payload: 'pause-all' }); });
  await waitFor(() => expect(state.taskAction).toHaveBeenCalledWith('queued', 'pause'));
  expect(state.taskAction).not.toHaveBeenCalledWith('active', 'pause');
  await act(async () => { waitingPause.resolve({ task: { ...queued, status: 'paused' } }); });
  await waitFor(() => expect(state.taskAction).toHaveBeenCalledWith('active', 'pause'));
  expect(await screen.findByRole('button', { name: '继续 waiting.zip' })).toBeVisible();
});

it('cancels one expanded torrent file without cancelling the parent task', async () => {
  const user = userEvent.setup();
  const task: QueueTask = {
    id: 'torrent-files', name: '合集', kind: 'torrent', status: 'downloading', progress: 25,
    files: [
      { index: 1, path: 'D://合集/episode.mkv', length: 100, completed_length: 25, selected: true },
      { index: 2, path: 'D://合集/readme.txt', length: 10, completed_length: 0, selected: true },
    ],
  };
  state.list.mockResolvedValueOnce([task]);
  state.cancelFile.mockResolvedValueOnce({
    ...task,
    files: [
      { ...task.files![0], selected: false },
      task.files![1],
    ],
  });
  render(<App />);

  await user.click(await screen.findByRole('button', { name: '展开 合集' }));
  await user.click(screen.getByRole('button', { name: '取消 episode.mkv 下载' }));

  await waitFor(() => expect(state.cancelFile).toHaveBeenCalledWith('torrent-files', 1));
  expect(state.taskAction).not.toHaveBeenCalledWith('torrent-files', 'cancel');
  expect(screen.getByRole('button', { name: '继续 episode.mkv' })).toBeVisible();
});

it('keeps live task events working while focus snapshots wait for native dragging to finish', async () => {
  render(<App />);
  await waitFor(() => expect(state.onFocus).toBeDefined());
  await waitFor(() => expect(state.list).toHaveBeenCalledWith('terminal'));
  state.list.mockClear();
  act(() => {
    state.listeners.get('main-window-interaction')?.({ payload: true });
    state.onFocus?.({ payload: true });
    state.onFocus?.({ payload: true });
    state.onTask?.(liveTask);
  });
  expect(await screen.findByRole('progressbar', { name: 'archive.iso 下载进度' })).toHaveAttribute('aria-valuenow', '40');
  await act(async () => { await new Promise((resolve) => setTimeout(resolve, 220)); });
  expect(state.list).not.toHaveBeenCalled();
  act(() => { state.listeners.get('main-window-interaction')?.({ payload: false }); });
  await waitFor(() => expect(state.list).toHaveBeenCalledWith('terminal'));
  expect(state.list.mock.calls.filter(([scope]) => scope === 'active')).toHaveLength(1);
});

it('refreshes an active HTTP task when a progress event is missed', async () => {
  vi.useFakeTimers();
  try {
    let snapshotPhase = 0;
    const submitted: QueueTask = { ...oldTask, progress: 0, completed_length: 0 };
    const progressed: QueueTask = { ...oldTask, progress: 40, completed_length: 40 };
    const completed: QueueTask = { ...oldTask, status: 'completed', progress: 100, completed_length: 100 };
    state.list.mockImplementation((scope?: string) => Promise.resolve(scope === 'active'
      ? snapshotPhase === 1 ? [progressed] : []
      : scope === 'terminal' && snapshotPhase === 2 ? [completed] : []
    ));
    render(<App />);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(state.onTask).toBeDefined();

    act(() => { state.onTask?.(submitted); });
    snapshotPhase = 1;
    await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });

    expect(screen.getByRole('progressbar', { name: 'archive.iso 下载进度' })).toHaveAttribute('aria-valuenow', '40');
    snapshotPhase = 2;
    await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
    fireEvent.click(screen.getByRole('tab', { name: /已完成 1/ }));
    expect(screen.getByRole('button', { name: '打开 archive.iso 所在目录' })).toBeVisible();
  } finally {
    vi.useRealTimers();
  }
});

it('reconciles an existing task from a later focus snapshot without dropping a new event task', async () => {
  let terminalRequests = 0;
  state.list.mockImplementation((scope?: string) => {
    if (scope !== 'terminal') return Promise.resolve([]);
    terminalRequests += 1;
    return Promise.resolve(terminalRequests === 1 ? [] : [{ ...liveTask, status: 'completed', progress: 100 }]);
  });
  render(<App />);
  await waitFor(() => expect(state.onFocus).toBeDefined());
  act(() => {
    state.onTask?.(liveTask);
    state.onTask?.({ id: 'new-task', name: 'new.zip', kind: 'file', status: 'downloading', progress: 20 });
    state.onFocus?.({ payload: true });
  });

  await waitFor(() => expect(screen.getByText('已完成')).toBeVisible());
  expect(within(screen.getByRole('region', { name: '下载队列' })).getByText('new.zip')).toBeVisible();
});

it('keeps a same-ID event update that arrives while a focus snapshot is pending', async () => {
  const focusSnapshot = deferred<QueueTask[]>();
  let activeRequests = 0;
  state.list.mockImplementation((scope?: string) => {
    if (scope !== 'active') return Promise.resolve([]);
    activeRequests += 1;
    return activeRequests === 1 ? Promise.resolve([]) : focusSnapshot.promise;
  });
  render(<App />);
  await waitFor(() => expect(state.onFocus).toBeDefined());
  act(() => { state.onFocus?.({ payload: true }); });
  await waitFor(() => expect(activeRequests).toBe(2));
  act(() => { state.onTask?.(liveTask); });
  await act(async () => { focusSnapshot.resolve([oldTask]); });

  expect(screen.getByRole('progressbar', { name: 'archive.iso 下载进度' })).toHaveAttribute('aria-valuenow', '40');
  expect(within(screen.getByRole('region', { name: '下载队列' })).getAllByText('archive.iso')).toHaveLength(1);
});

it('keeps the later requested list snapshot when focus responses arrive out of order', async () => {
  const older = deferred<QueueTask[]>();
  const newer = deferred<QueueTask[]>();
  let activeRequests = 0;
  state.list.mockImplementation((scope?: string) => {
    if (scope !== 'active') return Promise.resolve([]);
    activeRequests += 1;
    return activeRequests === 1 ? Promise.resolve([oldTask])
      : activeRequests === 2 ? older.promise : newer.promise;
  });
  render(<App />);
  await screen.findByText('archive.iso');
  await waitFor(() => expect(state.onFocus).toBeDefined());
  act(() => { state.onFocus?.({ payload: true }); });
  await waitFor(() => expect(activeRequests).toBe(2));
  act(() => { state.onFocus?.({ payload: true }); });
  await waitFor(() => expect(activeRequests).toBe(3));
  await act(async () => { newer.resolve([{ ...oldTask, progress: 65 }]); });
  await act(async () => { older.resolve([{ ...oldTask, progress: 20 }]); });

  expect(screen.getByRole('progressbar', { name: 'archive.iso 下载进度' })).toHaveAttribute('aria-valuenow', '65');
});

it('keeps an empty newer focus list when the older initial list later returns an unknown task', async () => {
  const initial = deferred<QueueTask[]>();
  let activeRequests = 0;
  state.list.mockImplementation((scope?: string) => {
    if (scope !== 'active') return Promise.resolve([]);
    return ++activeRequests === 1 ? initial.promise : Promise.resolve([]);
  });
  render(<App />);
  await waitFor(() => expect(state.onFocus).toBeDefined());
  await act(async () => { state.onFocus?.({ payload: true }); });
  await waitFor(() => expect(activeRequests).toBe(2));
  await act(async () => { initial.resolve([oldTask]); });

  expect(screen.queryByText('archive.iso')).not.toBeInTheDocument();
});

it('keeps a cloud directory member inside its authoritative root through member events and actions', async () => {
  const user = userEvent.setup();
  const root: QueueTask = {
    id: 'cloud-directory-root', name: 'Season 1', kind: 'folder', status: 'downloading', progress: 20,
    download_speed: 1024,
    children: [{ id: 'cloud-member-1', name: 'episode-01.mkv', kind: 'cloud', status: 'downloading', progress: 20, download_speed: 1024 }],
  };
  const memberEventRoot: QueueTask = {
    ...root, progress: 40, download_speed: 4096,
    children: [{ ...root.children![0], progress: 40, download_speed: 4096 }],
  };
  const actionRoot: QueueTask = {
    ...memberEventRoot, progress: 45, download_speed: 2048,
    children: [{ ...memberEventRoot.children![0], status: 'paused', progress: 45, download_speed: 0 }],
  };
  state.taskAction.mockResolvedValueOnce({
    task: { ...actionRoot.children![0] },
    queueTask: actionRoot,
  });
  render(<App />);
  await waitFor(() => expect(state.onTask).toBeDefined());

  act(() => { state.onTask?.(root); });
  await user.click(await screen.findByRole('button', { name: '展开 Season 1' }));
  expect(screen.getByText('episode-01.mkv')).toBeVisible();
  expect(document.querySelectorAll('.task-row')).toHaveLength(1);

  act(() => { state.onTask?.(memberEventRoot); });
  expect(screen.getByRole('progressbar', { name: 'Season 1 下载进度' })).toHaveAttribute('aria-valuenow', '40');

  await user.click(screen.getByRole('button', { name: '暂停 episode-01.mkv' }));
  await waitFor(() => expect(state.taskAction).toHaveBeenCalledWith('cloud-member-1', 'pause'));
  expect(screen.getByRole('progressbar', { name: 'Season 1 下载进度' })).toHaveAttribute('aria-valuenow', '45');
  expect(document.querySelectorAll('.task-row')).toHaveLength(1);
});

it('replaces a virtual failed directory leaf from its retry root without adding its new member at top level', async () => {
  const user = userEvent.setup();
  const root: QueueTask = {
    id: 'cloud-directory-root', name: 'Season 1', kind: 'folder', status: 'failed', progress: 0,
    children: [{ id: 'virtual-failed-leaf', name: 'unavailable.mkv', kind: 'cloud', status: 'failed', progress: 0, error: '链接生成失败' }],
  };
  const retriedRoot: QueueTask = {
    ...root, status: 'queued',
    children: [{ id: 'new-cloud-member', name: 'unavailable.mkv', kind: 'cloud', status: 'queued', progress: 0 }],
  };
  state.list.mockImplementation((scope?: string) => Promise.resolve(
    scope === 'terminal' ? [root] : []
  ));
  state.taskAction.mockResolvedValueOnce({
    task: { ...retriedRoot.children![0] },
    queueTask: retriedRoot,
  });
  render(<App />);
  await user.click(screen.getByRole('tab', { name: /失败/ }));

  await user.click(await screen.findByRole('button', { name: '展开 Season 1' }));
  expect(screen.getByText('链接生成失败')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '重新下载 unavailable.mkv' }));

  await waitFor(() => expect(state.taskAction).toHaveBeenCalledWith('virtual-failed-leaf', 'retry'));
  expect(screen.queryByText('链接生成失败')).not.toBeInTheDocument();
  await user.click(screen.getByRole('tab', { name: /下载中/ }));
  expect(document.querySelectorAll('.task-row')).toHaveLength(1);
  expect(screen.getByText('unavailable.mkv')).toBeVisible();
});

it('does not expose the obsolete queue confirmation shortcut for an awaiting magnet', async () => {
  const awaitingTask: QueueTask = {
    id: 'magnet-awaiting', name: 'collection', kind: 'magnet', status: 'awaiting_user', progress: 0,
    files: [{ index: 1, path: 'episode.mkv', length: 1024, completed_length: 0, selected: true }]
  };
  state.list.mockResolvedValueOnce([awaitingTask]);
  render(<App />);

  await screen.findByText('collection');
  expect(screen.queryByRole('button', { name: '确认下载 collection' })).not.toBeInTheDocument();
  expect(screen.queryByRole('dialog', { name: '确认下载内容' })).not.toBeInTheDocument();
  expect(state.selectFiles).not.toHaveBeenCalled();
});

it('fills a magnet confirmation dialog when its task files arrive later', async () => {
  state.list.mockResolvedValueOnce([]);
  state.parse.mockResolvedValueOnce({
    confirmation: { taskId: 'magnet-1', requiresConfirmation: true, resources: [], processing: [], isBitTorrent: true }
  });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await waitFor(() => expect(state.onTask).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'magnet:?xt=urn:btih:abc' }); });
  expect(screen.getByText('正在获取文件信息…')).toBeVisible();

  act(() => { state.onTask?.({
    id: 'magnet-1', name: 'collection', kind: 'magnet', status: 'awaiting_user', progress: 0,
    files: [{ index: 1, path: 'episode.mkv', length: 1024, completed_length: 0, selected: true }]
  }); });

  expect(screen.getByText('episode.mkv')).toBeVisible();
  expect(screen.queryByText('正在获取文件信息…')).not.toBeInTheDocument();
});

it('polls a pending magnet confirmation when a metadata event is missed', async () => {
  state.list
    .mockResolvedValueOnce([])
    .mockResolvedValue([{
      id: 'magnet-poll', name: 'collection', kind: 'magnet', status: 'awaiting_user', progress: 0,
      files: [{ index: 1, path: 'episode.mkv', length: 1024, completed_length: 0, selected: true }]
    }]);
  state.parse.mockResolvedValueOnce({
    confirmation: { taskId: 'magnet-poll', requiresConfirmation: true, resources: [], processing: [], isBitTorrent: true }
  });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'magnet:?xt=urn:btih:poll' }); });

  await waitFor(() => expect(screen.getByText('episode.mkv')).toBeVisible(), { timeout: 1200 });
  expect(screen.queryByText('正在获取文件信息…')).not.toBeInTheDocument();
});

it.each(['event', 'list'] as const)('keeps newer %s progress when an older metadata poll returns', async (source) => {
  const metadata = deferred<QueueTask[]>();
  const magnet: QueueTask = { ...oldTask, id: 'magnet-race', kind: 'magnet', name: 'collection', files: [] };
  let activeRequests = 0;
  state.list.mockImplementation((scope?: string) => {
    if (!scope) return metadata.promise;
    if (scope === 'terminal') return Promise.resolve([]);
    activeRequests += 1;
    return Promise.resolve(activeRequests === 1 ? [magnet] : [{ ...magnet, progress: 70 }]);
  });
  state.parse.mockResolvedValueOnce({
    confirmation: { taskId: magnet.id, requiresConfirmation: true, resources: [], processing: [], isBitTorrent: true }
  });
  render(<App />);
  await screen.findByText('collection');
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'magnet:?xt=urn:btih:race' }); });
  if (source === 'event') {
    act(() => { state.onTask?.({ ...magnet, progress: 70 }); });
  } else {
    act(() => { state.onFocus?.({ payload: true }); });
    await waitFor(() => expect(activeRequests).toBe(2));
  }
  await act(async () => { metadata.resolve([{ ...magnet, progress: 20,
    files: [{ index: 1, path: 'stale.mkv', length: 1024, completed_length: 0, selected: true }]
  }]); });

  expect(screen.getByRole('progressbar', { name: 'collection 下载进度' })).toHaveAttribute('aria-valuenow', '70');
  expect(screen.queryByText('stale.mkv')).not.toBeInTheDocument();
});

it('discards a closed magnet confirmation instead of leaving a queue recovery action', async () => {
  const user = userEvent.setup();
  state.list.mockResolvedValueOnce([]);
  state.parse.mockResolvedValueOnce({
    confirmation: { taskId: 'magnet-dismissed', requiresConfirmation: true, resources: [], processing: [], isBitTorrent: true }
  });
  render(<App />);
  await waitFor(() => expect(state.listeners.get('launcher-parse')).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'magnet:?xt=urn:btih:dismissed' }); });

  await user.click(screen.getByRole('button', { name: '关闭确认窗口' }));

  await waitFor(() => expect(state.discardPrepared).toHaveBeenCalledWith('magnet-dismissed'));
  expect(screen.queryByRole('dialog', { name: '确认下载内容' })).not.toBeInTheDocument();
});

it('preserves a user deselection when later magnet metadata refreshes the file', async () => {
  const user = userEvent.setup();
  state.list.mockResolvedValueOnce([]);
  state.parse.mockResolvedValueOnce({
    confirmation: { taskId: 'magnet-2', requiresConfirmation: true, processing: [], isBitTorrent: true,
      resources: [{ id: '1', index: 1, name: 'pending.mkv', size: '未知', selected: true }] }
  });
  render(<App />);
  await waitFor(() => expect(state.onTask).toBeDefined());
  await act(async () => { state.listeners.get('launcher-parse')?.({ payload: 'magnet:?xt=urn:btih:def' }); });
  await user.click(screen.getByRole('checkbox', { name: '选择 pending.mkv' }));

  act(() => { state.onTask?.({
    id: 'magnet-2', name: 'collection', kind: 'magnet', status: 'awaiting_user', progress: 0,
    files: [{ index: 1, path: 'renamed.mkv', length: 2048, completed_length: 1024, selected: true }]
  }); });

  expect(screen.getByText('renamed.mkv')).toBeVisible();
  expect(screen.getByRole('checkbox', { name: '选择 renamed.mkv' })).not.toBeChecked();
});

it('cleans up launcher listeners that resolve after App unmounts', async () => {
  const navigate = deferred<() => void>();
  const parse = deferred<() => void>();
  const stopNavigate = vi.fn();
  const stopParse = vi.fn();
  state.list.mockResolvedValueOnce([]);
  state.listen.mockImplementation((name) => {
    if (name === 'launcher-navigate') return navigate.promise;
    if (name === 'launcher-parse') return parse.promise;
    return Promise.resolve(() => undefined);
  });
  const { unmount } = render(<App />);

  unmount();
  await act(async () => {
    navigate.resolve(stopNavigate);
    parse.resolve(stopParse);
  });

  expect(stopNavigate).toHaveBeenCalledOnce();
  expect(stopParse).toHaveBeenCalledOnce();
});
