import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { readFileSync } from 'node:fs';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({
  getGlobalStat: vi.fn(), getSettings: vi.fn(), scanDownloadInputs: vi.fn(),
  updateSettings: vi.fn(), getManagerVisible: vi.fn(), getLauncherPopupPlacement: vi.fn(), getLauncherPopupSide: vi.fn(),
  setLauncherInputRegions: vi.fn(),
  runNativeLauncherDrag: vi.fn(),
  beginLauncherPointerDrag: vi.fn(), moveLauncherPointerDrag: vi.fn(), toggleManager: vi.fn(), openNewDownload: vi.fn(), stageDroppedTorrent: vi.fn(),
  onMoved: vi.fn(), onScaleChanged: vi.fn(),
  movedHandler: undefined as undefined | (() => void),
  scaleChangedHandler: undefined as undefined | (() => void),
  onGlobalStat: undefined as undefined | ((stat: { download_speed: number; num_active: number }) => void),
}));

vi.mock('../lib/api', () => ({
  isTauriRuntime: () => true, getDownloadGlobalStat: state.getGlobalStat,
  getSettings: state.getSettings, scanDownloadInputs: state.scanDownloadInputs, updateSettings: state.updateSettings,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn().mockResolvedValue(() => undefined) }));
vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({
    onMoved: state.onMoved,
    onScaleChanged: state.onScaleChanged,
  }),
}));
vi.mock('../lib/launcherWindow', () => ({
  launcherCanvas: { width: 756, height: 720, popupSlotWidth: 346, popupSlotHeight: 328, iconSize: 64, iconLeft: 346, iconTop: 328, margin: 24 },
  launcherIconRegion: { x: 346, y: 328, width: 64, height: 64 },
  setLauncherInputRegions: state.setLauncherInputRegions,
  getLauncherPopupPlacement: state.getLauncherPopupPlacement,
  getLauncherPopupSide: state.getLauncherPopupSide,
  anchorLauncherToWorkArea: vi.fn().mockResolvedValue(undefined),
  setLauncherWindowVisible: vi.fn().mockResolvedValue(undefined),
  runLauncherMenuAction: vi.fn().mockResolvedValue(undefined),
  getLauncherManagerVisible: state.getManagerVisible,
  showLauncherMenu: vi.fn().mockResolvedValue(undefined),
  beginLauncherPointerDrag: state.beginLauncherPointerDrag,
  runNativeLauncherDrag: state.runNativeLauncherDrag,
  moveLauncherPointerDrag: state.moveLauncherPointerDrag,
  toggleLauncherManager: state.toggleManager,
  openLauncherNewDownload: state.openNewDownload,
  stageDroppedTorrent: state.stageDroppedTorrent,
}));
vi.mock('../lib/downloadEvents', () => ({
  formatSpeed: (value: number) => value === 524_288 ? '512 KB/s' : `${value} B/s`,
  subscribeToDownloadEvents: vi.fn().mockImplementation(({ onGlobalStat }) => {
    state.onGlobalStat = onGlobalStat;
    return Promise.resolve(() => undefined);
  }),
}));

import { LauncherWindow } from './LauncherWindow';
import { beginLauncherPointerDrag, moveLauncherPointerDrag, setLauncherInputRegions, setLauncherWindowVisible } from '../lib/launcherWindow';

const icon = { x: 346, y: 328, width: 64, height: 64 };
const menu = { x: 114, y: 16, width: 280, height: 296 };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((yes) => { resolve = yes; });
  return { promise, resolve };
}

beforeEach(() => {
  vi.clearAllMocks();
  state.onGlobalStat = undefined;
  state.getGlobalStat.mockReset().mockResolvedValue({ download_speed: 524_288, num_active: 2 });
  state.getSettings.mockReset().mockResolvedValue({ launcherVisibility: 'always' });
  state.scanDownloadInputs.mockReset();
  state.updateSettings.mockReset();
  state.getManagerVisible.mockReset().mockResolvedValue(true);
  state.getLauncherPopupPlacement.mockReset().mockResolvedValue('above');
  state.getLauncherPopupSide.mockReset().mockResolvedValue('left');
  state.setLauncherInputRegions.mockReset().mockResolvedValue(undefined);
  state.runNativeLauncherDrag.mockReset().mockResolvedValue(false);
  state.beginLauncherPointerDrag.mockReset().mockResolvedValue({
    windowPosition: { x: 1369, y: 506 }, pointerPosition: { x: 10, y: 10 }, scaleFactor: 1.5,
  });
  state.moveLauncherPointerDrag.mockReset().mockResolvedValue(undefined);
  state.toggleManager.mockReset().mockResolvedValue(true);
  state.openNewDownload.mockReset().mockResolvedValue(undefined);
  state.stageDroppedTorrent.mockReset().mockResolvedValue('C:\\AppData\\dropped-torrents\\fixture.torrent');
  state.movedHandler = undefined;
  state.scaleChangedHandler = undefined;
  state.onMoved.mockReset().mockImplementation((handler) => {
    state.movedHandler = handler;
    return Promise.resolve(() => { state.movedHandler = undefined; });
  });
  state.onScaleChanged.mockReset().mockImplementation((handler) => {
    state.scaleChangedHandler = handler;
    return Promise.resolve(() => { state.scaleChangedHandler = undefined; });
  });
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function () {
    const bounds = this.classList.contains('launcher-context-menu') ? menu : { x: 0, y: 0, width: 0, height: 0 };
    return { ...bounds, left: bounds.x, top: bounds.y, right: bounds.x + bounds.width, bottom: bounds.y + bounds.height, toJSON() {} };
  });
});

afterEach(() => vi.mocked(HTMLElement.prototype.getBoundingClientRect).mockRestore());

it('keeps the floating-display menu heading to one row after its helper copy is removed', () => {
  expect(readFileSync('src/styles.css', 'utf8').includes('.launcher-context-menu>header button{grid-column:2;grid-row:1;')).toBe(true);
});

it('keeps the hexagon at the same fixed-canvas coordinate for both popup placements', () => {
  const styles = readFileSync('src/styles.css', 'utf8');
  expect(styles.includes('.launcher-shell .launcher-interaction-target{position:absolute;left:346px;top:328px;')).toBe(true);
  expect(styles.includes('.launcher-popup-below .launcher-interaction-target')).toBe(false);
  expect(styles.includes('.launcher-popup-right .launcher-interaction-target')).toBe(false);
  expect(styles.includes('launcher-viewport-transitioning')).toBe(false);
});

it.each(['clear-blue', 'mint', 'warm-gold', 'lavender', 'coral', 'deep-ocean', 'night-violet'])('keeps the launcher viewport transparent with the %s theme', (theme) => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  const themeCanvas = document.createElement('style');
  themeCanvas.textContent = `:root[data-theme="${theme}"]{background:rgb(1,2,3)!important}`;
  document.head.append(themeCanvas, stylesheet);
  document.documentElement.classList.add('launcher-window');
  document.documentElement.dataset.theme = theme;
  try {
    expect(getComputedStyle(document.documentElement).backgroundColor).toBe('rgba(0, 0, 0, 0)');
    expect(getComputedStyle(document.body).backgroundColor).toBe('rgba(0, 0, 0, 0)');
  } finally {
    document.documentElement.classList.remove('launcher-window');
    delete document.documentElement.dataset.theme;
    themeCanvas.remove(); stylesheet.remove();
  }
});

it('requests an initial global-stat snapshot after subscribing and keeps only the hexagon', async () => {
  render(<LauncherWindow />);
  await waitFor(() => expect(state.getGlobalStat).toHaveBeenCalledOnce());
  expect(state.onGlobalStat).toBeDefined();
  expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]);
  expect(screen.queryByLabelText('下载速度 512 KB/s')).not.toBeInTheDocument();
});

it('installs safe input before applying startup visibility', async () => {
  const pending = deferred<void>();
  state.setLauncherInputRegions.mockReturnValueOnce(pending.promise);
  render(<LauncherWindow />);
  await waitFor(() => expect(state.getGlobalStat).toHaveBeenCalled());
  expect(setLauncherWindowVisible).not.toHaveBeenCalled();
  await act(async () => pending.resolve());
  await waitFor(() => expect(setLauncherWindowVisible).toHaveBeenCalledWith(true));
});

it('keeps the compact launcher visible for an active task in download-only mode', async () => {
  state.getSettings.mockResolvedValueOnce({ launcherVisibility: 'downloading' });
  render(<LauncherWindow />);
  await waitFor(() => expect(setLauncherWindowVisible).toHaveBeenLastCalledWith(true));
});

it('keeps the hexagon compact on one click, toggles the manager on double click, and opens the context menu on right click', async () => {
  render(<LauncherWindow />);
  const trigger = screen.getByRole('button', { name: '打开下载输入' });
  const mark = trigger.querySelector('.launcher-mark');
  fireEvent.click(trigger);
  expect(screen.queryByRole('dialog', { name: '新建下载' })).not.toBeInTheDocument();
  fireEvent.doubleClick(trigger);
  await waitFor(() => expect(state.toggleManager).toHaveBeenCalledOnce());
  fireEvent.contextMenu(trigger);
  expect(await screen.findByRole('menu', { name: '悬浮球菜单' })).toBeVisible();
  expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon, menu]);
  expect(trigger.querySelector('.launcher-mark')).toBe(mark);
});

it('opens the self-drawn menu when WebKitGTK forwards a native context-menu signal', async () => {
  render(<LauncherWindow />);

  window.dispatchEvent(new Event('versadown:launcher-context-menu'));

  expect(await screen.findByRole('menu', { name: '悬浮球菜单' })).toBeVisible();
  expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon, menu]);
});

it('closes the launcher context menu after a left click on the floating trigger', async () => {
  render(<LauncherWindow />);
  const trigger = screen.getByRole('button', { name: '打开下载输入' });

  fireEvent.contextMenu(trigger);
  await screen.findByRole('menu', { name: '悬浮球菜单' });
  fireEvent.click(trigger);

  await waitFor(() => expect(screen.queryByRole('menu', { name: '悬浮球菜单' })).not.toBeInTheDocument());
});

it('never resizes, clips, hides, or shows the native window for context-menu transitions', async () => {
  render(<LauncherWindow />);
  await waitFor(() => expect(setLauncherWindowVisible).toHaveBeenCalledOnce());
  const trigger = screen.getByRole('button', { name: '打开下载输入' });
  fireEvent.contextMenu(trigger);
  await screen.findByRole('menu');
  fireEvent.keyDown(window, { key: 'Escape' });
  await waitFor(() => expect(screen.queryByRole('menu')).not.toBeInTheDocument());
  expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]);
  expect(vi.mocked(setLauncherWindowVisible).mock.calls.slice(1)).toEqual([]);
});

it('uses the last intent while an earlier placement lookup is pending', async () => {
  render(<LauncherWindow />);
  await waitFor(() => expect(state.getLauncherPopupPlacement).toHaveBeenCalled());
  state.getLauncherPopupPlacement.mockClear();
  const pending = deferred<'above' | 'below'>();
  state.getLauncherPopupPlacement.mockReturnValueOnce(pending.promise);
  const trigger = screen.getByRole('button', { name: '打开下载输入' });
  fireEvent.contextMenu(trigger);
  await waitFor(() => expect(state.getLauncherPopupPlacement).toHaveBeenCalledOnce());
  fireEvent.keyDown(window, { key: 'Escape' });
  await act(async () => pending.resolve('below'));
  expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]);
});

it('does not open a new-task dialog when left and right clicks alternate rapidly', async () => {
  render(<LauncherWindow />);
  const trigger = screen.getByRole('button', { name: '打开下载输入' });
  fireEvent.click(trigger); fireEvent.contextMenu(trigger); fireEvent.click(trigger);
  expect(screen.queryByRole('dialog', { name: '新建下载' })).not.toBeInTheDocument();
  await waitFor(() => expect(screen.queryByRole('menu', { name: '悬浮球菜单' })).not.toBeInTheDocument());
});

it.each(['Escape', 'blur', 'drag'] as const)('restores icon-only input for %s', async (reason) => {
  render(<LauncherWindow />);
  const trigger = screen.getByRole('button', { name: '打开下载输入' });
  fireEvent.contextMenu(trigger);
  await screen.findByRole('menu');
  if (reason === 'Escape') fireEvent.keyDown(window, { key: 'Escape' });
  if (reason === 'blur') fireEvent.blur(window);
  if (reason === 'drag') {
    fireEvent.mouseDown(trigger, { button: 0, buttons: 1, clientX: 10, clientY: 10, screenX: 100, screenY: 200 });
    fireEvent.mouseMove(trigger, { buttons: 1, clientX: 30, clientY: 30, screenX: 120, screenY: 220 });
  }
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]));
  await waitFor(() => expect(screen.queryByRole('menu')).not.toBeInTheDocument());
});

it('serializes region writes so a slow popup update cannot override compact input', async () => {
  render(<LauncherWindow />);
  await waitFor(() => expect(setLauncherWindowVisible).toHaveBeenCalled());
  const pending = deferred<void>();
  state.setLauncherInputRegions.mockReturnValueOnce(pending.promise);
  fireEvent.contextMenu(screen.getByRole('button', { name: '打开下载输入' }));
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon, menu]));
  fireEvent.keyDown(window, { key: 'Escape' });
  await waitFor(() => expect(screen.queryByRole('menu')).not.toBeInTheDocument());
  expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon, menu]);
  await act(async () => pending.resolve());
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]));
});

it('begins a captured pointer drag before a slow compact input-region update', async () => {
  render(<LauncherWindow />);
  const trigger = screen.getByRole('button', { name: '打开下载输入' });
  fireEvent.contextMenu(trigger);
  await screen.findByRole('menu');
  const pending = deferred<void>();
  state.setLauncherInputRegions.mockReturnValueOnce(pending.promise);
  state.getLauncherPopupPlacement.mockClear();
  fireEvent.mouseDown(trigger, { button: 0, buttons: 1, clientX: 10, clientY: 10, screenX: 100, screenY: 200 });
  fireEvent.mouseMove(trigger, { buttons: 1, clientX: 30, clientY: 30, screenX: 120, screenY: 220 });
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]));
  await waitFor(() => expect(beginLauncherPointerDrag).toHaveBeenCalledWith(
    { x: 100, y: 200 },
    { x: 10, y: 10 },
  ));
  await waitFor(() => expect(moveLauncherPointerDrag).toHaveBeenCalledWith(expect.anything(), { x: 120, y: 220 }));
  await act(async () => pending.resolve());
});

it('lets native dragging own continuous movement and release without competing browser positions', async () => {
  const release = deferred<boolean>();
  state.runNativeLauncherDrag.mockReturnValueOnce(release.promise);
  render(<LauncherWindow />);
  await waitFor(() => expect(setLauncherWindowVisible).toHaveBeenCalled());
  state.setLauncherInputRegions.mockClear();
  const trigger = screen.getByRole('button', { name: '打开下载输入' });
  fireEvent.mouseDown(trigger, { button: 0, screenX: 100, screenY: 200 });
  fireEvent.mouseMove(trigger, { buttons: 1, screenX: 120, screenY: 220 });
  await waitFor(() => expect(state.runNativeLauncherDrag).toHaveBeenCalledOnce());
  for (let x = 150; x <= 500; x += 50) {
    fireEvent.mouseMove(trigger, { buttons: 1, screenX: x, screenY: 250, clientX: 10 });
    act(() => state.movedHandler?.());
  }
  fireEvent.mouseUp(trigger, { button: 0, screenX: 500, screenY: 250 });
  expect(state.beginLauncherPointerDrag).not.toHaveBeenCalled();
  expect(state.moveLauncherPointerDrag).not.toHaveBeenCalled();
  await act(async () => release.resolve(true));
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]));
  expect(state.moveLauncherPointerDrag).not.toHaveBeenCalled();
  // Subsequent drags remain available after release.
  state.runNativeLauncherDrag.mockResolvedValueOnce(true);
  fireEvent.mouseDown(trigger, { button: 0, screenX: 500, screenY: 250 });
  fireEvent.mouseMove(trigger, { buttons: 1, screenX: 520, screenY: 250 });
  await waitFor(() => expect(state.runNativeLauncherDrag).toHaveBeenCalledTimes(2));
});

it('retains the previous input on failure and never falls back to the full canvas', async () => {
  render(<LauncherWindow />);
  await waitFor(() => expect(setLauncherWindowVisible).toHaveBeenCalled());
  state.setLauncherInputRegions.mockRejectedValueOnce(new Error('native update failed'));
  fireEvent.contextMenu(screen.getByRole('button', { name: '打开下载输入' }));
  await screen.findByRole('menu');
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon, menu]));
  fireEvent.keyDown(window, { key: 'Escape' });
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon]));
  expect(vi.mocked(setLauncherInputRegions).mock.calls.every(([regions]) => regions.length <= 2 && regions[0].width === 64)).toBe(true);
});

it('keeps the previous native input region when a later update fails', async () => {
  state.setLauncherInputRegions
    .mockResolvedValueOnce(undefined)
    .mockRejectedValueOnce(new Error('native failure'));
  render(<LauncherWindow />);
  const trigger = await screen.findByRole('button', { name: '打开下载输入' });
  fireEvent.contextMenu(trigger);
  await waitFor(() => expect(setLauncherInputRegions).toHaveBeenCalledTimes(2));
  expect(setLauncherInputRegions).not.toHaveBeenCalledWith([
    { x: 0, y: 0, width: 410, height: 720 },
  ]);
});

it('changes native visibility only for the visibility policy, not popup state', async () => {
  render(<LauncherWindow />);
  await screen.findByRole('button', { name: '打开下载输入' });
  await waitFor(() => expect(setLauncherWindowVisible).toHaveBeenCalled());
  vi.mocked(setLauncherWindowVisible).mockClear();
  await act(async () => {
    fireEvent.contextMenu(screen.getByRole('button', { name: '打开下载输入' }));
  });
  await screen.findByRole('menu', { name: '悬浮球菜单' });
  expect(setLauncherWindowVisible).not.toHaveBeenCalled();
});

it('recomputes placement and reapplies the safe input region after the launcher moves', async () => {
  render(<LauncherWindow />);
  const trigger = await screen.findByRole('button', { name: '打开下载输入' });
  fireEvent.contextMenu(trigger);
  await screen.findByRole('menu', { name: '悬浮球菜单' });
  state.getLauncherPopupPlacement.mockResolvedValue('below');
  await act(async () => state.movedHandler?.());
  await waitFor(() => expect(trigger.closest('main')).toHaveClass('launcher-popup-below'));
  expect(setLauncherInputRegions).toHaveBeenLastCalledWith([icon, menu]);
});

it('uses the below slot for the context menu without moving the icon', async () => {
  state.getLauncherPopupPlacement.mockResolvedValue('below');
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<LauncherWindow />);
    const trigger = screen.getByRole('button', { name: '打开下载输入' });
    fireEvent.contextMenu(trigger);
    const surface = await screen.findByRole('menu');
    expect(trigger.closest('main')).toHaveClass('launcher-popup-below');
    expect(getComputedStyle(trigger.closest('.launcher-interaction-target')!).top).toBe('328px');
    expect(getComputedStyle(surface).top).toBe('392px');
  } finally { stylesheet.remove(); }
});

it('opens the context menu into the right slot near the left screen edge', async () => {
  state.getLauncherPopupSide.mockResolvedValue('right');
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const { container } = render(<LauncherWindow />);
    const target = container.querySelector('.launcher-interaction-target')!;
    fireEvent.contextMenu(container.querySelector('.launcher-trigger')!);
    const surface = await waitFor(() => {
      const menu = container.querySelector<HTMLElement>('.launcher-context-menu');
      expect(menu).not.toBeNull();
      return menu!;
    });
    expect(container.querySelector('main')).toHaveClass('launcher-popup-right');
    expect(getComputedStyle(target).left).toBe('346px');
    expect(getComputedStyle(surface).left).toBe('346px');
    expect(getComputedStyle(surface).right).toBe('auto');
  } finally { stylesheet.remove(); }
});

it('shows current speed, manager visibility, and the display settings menu', async () => {
  state.getManagerVisible.mockResolvedValue(false);
  const user = userEvent.setup();
  render(<LauncherWindow />);
  fireEvent.contextMenu(screen.getByRole('button', { name: '打开下载输入' }));
  const menuElement = await screen.findByRole('menu', { name: '悬浮球菜单' });
  expect(menuElement).toHaveClass('task-context-menu');
  expect(screen.getByLabelText('当前整体下载速度')).toHaveTextContent('512 KB/s');
  expect(await screen.findByRole('menuitem', { name: '显示主界面' })).toBeVisible();
  await user.click(screen.getByRole('menuitem', { name: '悬浮球显示' }));
  expect(await screen.findByRole('menu', { name: '悬浮球显示设置' })).toBeVisible();
  expect(screen.queryByText('选择显示时机')).not.toBeInTheDocument();
  expect(screen.getByRole('menuitemradio', { name: '总是显示' })).toHaveAttribute('aria-checked', 'true');
  expect(screen.getByRole('menuitemradio', { name: '仅下载时显示' })).toBeVisible();
  expect(screen.getByRole('menuitemradio', { name: '关闭显示' })).toBeVisible();
});
