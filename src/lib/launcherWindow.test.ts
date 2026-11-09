import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeEach, expect, it, vi } from 'vitest';

const native = vi.hoisted(() => {
  const calls: string[] = [];
  return {
    calls,
    invoke: vi.fn(),
    emitTo: vi.fn(async () => { calls.push('emit'); }),
    getByLabel: vi.fn(),
    currentMonitor: vi.fn(),
    availableMonitors: vi.fn(),
    launcherPosition: vi.fn(),
    launcherSize: vi.fn(),
    setSize: vi.fn().mockResolvedValue(undefined),
    setPosition: vi.fn().mockResolvedValue(undefined),
    mainWindow: {
      unminimize: vi.fn(async () => { calls.push('unminimize'); }),
      show: vi.fn(async () => { calls.push('show'); }),
      setFocus: vi.fn(async () => { calls.push('focus'); })
    }
  };
});

vi.mock('./api', () => ({ isTauriRuntime: () => true }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ emitTo: native.emitTo }));
vi.mock('@tauri-apps/api/window', () => ({
  currentMonitor: native.currentMonitor,
  availableMonitors: native.availableMonitors,
  getCurrentWindow: vi.fn(() => ({ label: 'launcher', outerPosition: native.launcherPosition, outerSize: native.launcherSize, setSize: native.setSize, setPosition: native.setPosition })),
  LogicalSize: class LogicalSize {
    constructor(public width: number, public height: number) {}
  },
  PhysicalPosition: class PhysicalPosition {
    constructor(public x: number, public y: number) {}
  },
  PhysicalSize: class PhysicalSize {
    constructor(public width: number, public height: number) {}
  },
  Window: { getByLabel: native.getByLabel }
}));

import * as launcherWindow from './launcherWindow';

beforeEach(() => {
  native.calls.length = 0;
  vi.clearAllMocks();
  native.getByLabel.mockResolvedValue(native.mainWindow);
  native.currentMonitor.mockReset();
  native.launcherPosition.mockReset();
  native.launcherSize.mockReset();
  native.setSize.mockReset().mockResolvedValue(undefined);
});

it('uses one fixed canvas with an invariant hexagon anchor', () => {
  expect(launcherWindow.launcherCanvas).toEqual({
    width: 756,
    height: 720,
    popupSlotWidth: 346,
    popupSlotHeight: 328,
    iconSize: 64,
    iconLeft: 346,
    iconTop: 328,
    margin: 24,
  });
});

it('does not expose native resize or visual clipping helpers to popup code', () => {
  const source = readFileSync(resolve('src/lib/launcherWindow.ts'), 'utf8');
  expect(source).not.toContain('set_launcher_bounds');
  expect(source).not.toContain('set_launcher_compact_shape');
  expect(source).not.toContain('resizeLauncher');
});

it('installs the compact icon input region without changing native bounds', async () => {
  await launcherWindow.setLauncherInputRegions([{ x: 346, y: 328, width: 64, height: 64 }]);
  expect(native.invoke).toHaveBeenCalledWith('set_launcher_input_regions', {
    regions: [{ x: 346, y: 328, width: 64, height: 64 }],
    scaleFactor: 1,
  });
  expect(native.invoke).not.toHaveBeenCalledWith('set_launcher_bounds', expect.anything());
});

it('rejects malformed logical input regions before invoking the native adapter', async () => {
  await expect(launcherWindow.setLauncherInputRegions([
    { x: 346, y: 328, width: Number.NaN, height: 64 },
  ])).rejects.toThrow('悬浮窗输入区域无效');
  expect(native.invoke).not.toHaveBeenCalledWith('set_launcher_input_regions', expect.anything());
});

it('positions the fixed canvas without consulting a variable native window size', async () => {
  native.currentMonitor.mockResolvedValueOnce({
    scaleFactor: 1.5,
    workArea: { position: { x: 100, y: 50 }, size: { width: 1920, height: 1080 } },
  });
  native.launcherSize.mockResolvedValueOnce({ width: 64, height: 64 });
  await launcherWindow.anchorLauncherToWorkArea();
  expect(native.setSize).toHaveBeenCalledWith({ width: 1134, height: 1080 });
  expect(native.setPosition).toHaveBeenCalledWith({ x: 1369, y: 506 });
  expect(native.launcherSize).not.toHaveBeenCalled();
});

it('uses the WebView pixel ratio when a Linux monitor reports an unscaled factor', async () => {
  const original = Object.getOwnPropertyDescriptor(window, 'devicePixelRatio');
  Object.defineProperty(window, 'devicePixelRatio', { configurable: true, value: 1.5 });
  native.currentMonitor.mockResolvedValueOnce({
    scaleFactor: 1,
    workArea: { position: { x: 100, y: 50 }, size: { width: 1920, height: 1080 } },
  });

  try {
    await launcherWindow.anchorLauncherToWorkArea();
    expect(native.setPosition).toHaveBeenCalledWith({ x: 1369, y: 506 });
  } finally {
    if (original) Object.defineProperty(window, 'devicePixelRatio', original);
    else Reflect.deleteProperty(window, 'devicePixelRatio');
  }
});

it.each([
  [{ x: 100, y: -400 }, 'below'],
  [{ x: 100, y: 100 }, 'above'],
] as const)('chooses placement from the invariant icon at %j', async (position, expected) => {
  native.launcherPosition.mockResolvedValueOnce(position);
  native.currentMonitor.mockResolvedValueOnce({
    scaleFactor: 1.5,
    workArea: { position: { x: 0, y: 0 }, size: { width: 1920, height: 1080 } },
  });
  await expect(launcherWindow.getLauncherPopupPlacement()).resolves.toBe(expected);
  expect(native.launcherSize).not.toHaveBeenCalled();
});

it('anchors the invariant hexagon to the work-area bottom right', () => {
  expect(launcherWindow.launcherCanvasPosition(
    { x: 100, y: 50, width: 1920, height: 1080 },
    1.5,
  )).toEqual({ x: 1369, y: 506 });
});

it('maps the fixed hexagon to a physical screen rectangle', () => {
  expect(launcherWindow.launcherIconScreenRect({ x: 100, y: 50 }, 1.5)).toEqual({
    x: 619,
    y: 542,
    width: 96,
    height: 96,
  });
});

it('moves the fixed canvas above the work area when a captured icon drag reaches the desktop top', () => {
  expect(launcherWindow.launcherPointerDragPosition(
    {
      windowPosition: { x: 1369, y: 506 },
      pointerPosition: { x: 378, y: 360 },
      scaleFactor: 1.5,
    },
    { x: 378, y: -292 },
  )).toEqual({ x: 1369, y: -472 });
});

it('keeps WebKitGTK physical screen-pointer deltas unscaled at fractional DPI', () => {
  expect(launcherWindow.launcherPointerCoordinateScaleFactor(
    { x: 1909, y: 520 },
    { x: 2476, y: 1060 },
    { x: 378, y: 360 },
    1.5,
  )).toBe(1);
});

it('chooses the lower slot without moving the hexagon when upper space is insufficient', () => {
  const icon = { x: 850, y: 20, width: 64, height: 64 };
  expect(launcherWindow.launcherPopupPlacementFromIcon(
    icon,
    328,
    { x: 0, y: 0, width: 1000, height: 800 },
  )).toBe('below');
});

it('prefers the upper slot when the complete popup fits', () => {
  const icon = { x: 850, y: 500, width: 64, height: 64 };
  expect(launcherWindow.launcherPopupPlacementFromIcon(
    icon,
    328,
    { x: 0, y: 0, width: 1000, height: 800 },
  )).toBe('above');
});

it('uses the native visibility command so background smoke runs cannot surface a launcher', async () => {
  await launcherWindow.setLauncherWindowVisible(false);

  expect(native.invoke).toHaveBeenCalledWith('set_launcher_window_visible', { visible: false });
});

it('reveals the manager through the native path without forwarding a parse request', async () => {
  await launcherWindow.openLauncherManager();

  // Tao's unminimize/show/setFocus cannot restore a minimized manager on X11
  // window managers that keep it mapped; the native path also presents it.
  expect(native.invoke).toHaveBeenCalledWith('reveal_manager_from_launcher');
  expect(native.calls).toEqual([]);
  expect(native.emitTo).not.toHaveBeenCalled();
});

it('lets the native side toggle the manager on a launcher double click', async () => {
  native.invoke.mockResolvedValueOnce(false);

  await expect(launcherWindow.toggleLauncherManager()).resolves.toBe(false);

  expect(native.invoke).toHaveBeenCalledWith('toggle_manager_from_launcher');
  // The renderer cannot tell whether the manager was in front: pressing the
  // launcher has already taken its focus. It must not show or focus it itself.
  expect(native.calls).toEqual([]);
  expect(native.emitTo).not.toHaveBeenCalled();
});

it('waits for native drag completion without sending browser position updates', async () => {
  native.invoke.mockResolvedValueOnce(true);
  await expect(launcherWindow.runNativeLauncherDrag({ x: 378, y: 360 })).resolves.toBe(true);
  expect(native.invoke).toHaveBeenCalledWith('drag_launcher_window', { grabOffset: { x: 378, y: 360 } });
  expect(native.setPosition).not.toHaveBeenCalled();
});

it('opens the manager new-task dialog with an accepted launcher drop', async () => {
  await launcherWindow.openLauncherNewDownload('magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567');

  expect(native.invoke).toHaveBeenCalledWith('reveal_manager_from_launcher');
  // The manager is revealed before the new-download request is forwarded.
  expect(native.calls).toEqual(['emit']);
  expect(native.emitTo).toHaveBeenCalledWith(
    'main',
    'launcher-new-download',
    'magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567'
  );
});

// 2560x1600 at 1.5x with a 60px bottom dock: the icon is 96px physical and
// sits at (519, 492) inside the 615x1080 canvas.
const primaryWorkArea = { x: 0, y: 0, width: 2560, height: 1540 };
const dragBounds = { workAreas: [primaryWorkArea], displayScaleFactor: 1.5 };

it('stops a dragged icon with its top edge on the top of the work area', () => {
  expect(launcherWindow.clampLauncherWindowPosition({ x: 1369, y: -900 }, dragBounds))
    .toEqual({ x: 1369, y: -492 });
});

it('keeps the dragged icon inside the left, right and bottom work-area edges', () => {
  expect(launcherWindow.clampLauncherWindowPosition({ x: -900, y: 300 }, dragBounds))
    .toEqual({ x: -519, y: 300 });
  expect(launcherWindow.clampLauncherWindowPosition({ x: 2400, y: 300 }, dragBounds))
    .toEqual({ x: 2560 - 96 - 519, y: 300 });
  expect(launcherWindow.clampLauncherWindowPosition({ x: 1369, y: 1500 }, dragBounds))
    .toEqual({ x: 1369, y: 1540 - 96 - 492 });
});

it('lets the icon move onto a second monitor and clamps it there', () => {
  const bounds = {
    workAreas: [primaryWorkArea, { x: 2560, y: 0, width: 1920, height: 1080 }],
    displayScaleFactor: 1.5,
  };
  expect(launcherWindow.clampLauncherWindowPosition({ x: 2800, y: 100 }, bounds))
    .toEqual({ x: 2800, y: 100 });
  expect(launcherWindow.clampLauncherWindowPosition({ x: 2800, y: 900 }, bounds))
    .toEqual({ x: 2800, y: 1080 - 96 - 492 });
});

it('clamps captured pointer drags when the session carries work-area bounds', () => {
  expect(launcherWindow.launcherPointerDragPosition(
    {
      windowPosition: { x: 1369, y: 506 },
      pointerPosition: { x: 378, y: 360 },
      scaleFactor: 1.5,
      bounds: dragBounds,
    },
    { x: 378, y: -400 },
  )).toEqual({ x: 1369, y: -492 });
});

it('records every monitor work area when a pointer drag begins', async () => {
  native.launcherPosition.mockResolvedValueOnce({ x: 1369, y: 506 });
  native.currentMonitor.mockResolvedValueOnce({ scaleFactor: 1.5 });
  native.availableMonitors.mockResolvedValueOnce([
    { workArea: { position: { x: 0, y: 0 }, size: { width: 2560, height: 1540 } } },
  ]);

  const session = await launcherWindow.beginLauncherPointerDrag({ x: 378, y: 360 });

  expect(session.bounds).toEqual(dragBounds);
});

it('reserves an equal popup slot on both sides of the icon', () => {
  const { width, popupSlotWidth, iconLeft, iconSize } = launcherWindow.launcherCanvas;
  expect(iconLeft).toBe(popupSlotWidth);
  expect(width).toBe(popupSlotWidth * 2 + iconSize);
  expect(launcherWindow.launcherIconRegion).toEqual({ x: 346, y: 328, width: 64, height: 64 });
});

it('opens the popup to the left when the complete popup fits there', () => {
  const icon = { x: 850, y: 500, width: 96, height: 96 };
  expect(launcherWindow.launcherPopupSideFromIcon(icon, 432, { x: 0, y: 0, width: 2560, height: 1540 }))
    .toBe('left');
});

it('opens the popup to the right when the icon is near the left work-area edge', () => {
  const icon = { x: 0, y: 500, width: 96, height: 96 };
  expect(launcherWindow.launcherPopupSideFromIcon(icon, 432, { x: 0, y: 0, width: 2560, height: 1540 }))
    .toBe('right');
});

it('reads the popup side from the current launcher position', async () => {
  native.launcherPosition.mockResolvedValueOnce({ x: -519, y: 300 });
  native.currentMonitor.mockResolvedValueOnce({
    scaleFactor: 1.5,
    workArea: { position: { x: 0, y: 0 }, size: { width: 2560, height: 1540 } },
  });
  await expect(launcherWindow.getLauncherPopupSide()).resolves.toBe('right');
});
