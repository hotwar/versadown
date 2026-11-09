import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { FloatingLauncher, type LauncherPointerPosition } from './FloatingLauncher';
import { LauncherContextMenu, type LauncherMenuAction } from './LauncherContextMenu';
import { anchorLauncherToWorkArea, beginLauncherPointerDrag, getLauncherManagerVisible, getLauncherPopupPlacement, getLauncherPopupSide, launcherIconRegion, moveLauncherPointerDrag, openLauncherNewDownload, toggleLauncherManager, runLauncherMenuAction, setLauncherInputRegions, setLauncherWindowVisible, showLauncherMenu, type LauncherInputRegion, type LauncherPointerDragSession, type LauncherPopupPlacement, type LauncherPopupSide } from '../lib/launcherWindow';
import { getDownloadGlobalStat, getSettings, isTauriRuntime, scanDownloadInputs, stageDroppedTorrent, updateSettings } from '../lib/api';
import { runNativeLauncherDrag } from '../lib/launcherWindow';
import { formatSpeed, subscribeToDownloadEvents } from '../lib/downloadEvents';
import type { DownloadGlobalStat, LauncherVisibility } from '../lib/types';
import { useTranslation } from '../i18n';

export type LauncherSurfaceMode = 'compact' | 'context-menu';

export function LauncherWindow() {
  const { t } = useTranslation();
  const [activeCount, setActiveCount] = useState(0);
  const [downloadSpeed, setDownloadSpeed] = useState(0);
  const [mode, setMode] = useState<LauncherSurfaceMode>('compact');
  const [popupPlacement, setPopupPlacement] = useState<LauncherPopupPlacement>('above');
  const popupPlacementRef = useRef<LauncherPopupPlacement>('above');
  const [popupSide, setPopupSide] = useState<LauncherPopupSide>('left');
  const popupSideRef = useRef<LauncherPopupSide>('left');
  const [managerVisible, setManagerVisible] = useState(true);
  const [launcherVisibility, setLauncherVisibility] = useState<LauncherVisibility>('always');
  const [settingsReady, setSettingsReady] = useState(false);
  const [anchored, setAnchored] = useState(false);
  const statEventEpoch = useRef(0);
  const requestedMode = useRef<LauncherSurfaceMode>('compact');
  const transitionQueue = useRef(Promise.resolve());
  const regionQueue = useRef(Promise.resolve());
  const lastValidRegions = useRef<LauncherInputRegion[]>([launcherIconRegion]);
  const disposed = useRef(false);
  const menuElement = useRef<HTMLElement | null>(null);
  const [inputReady, setInputReady] = useState(false);
  const dragSession = useRef<LauncherPointerDragSession | null>(null);
  const dragPendingPointer = useRef<LauncherPointerPosition | null>(null);
  const dragStartQueue = useRef(Promise.resolve());
  const dragMoveCompletion = useRef(Promise.resolve());
  const dragMoveInFlight = useRef(false);
  const launcherDragging = useRef(false);
  const nativeDragInFlight = useRef(false);
  const dragEndPointer = useRef<LauncherPointerPosition | null>(null);
  popupPlacementRef.current = popupPlacement;
  popupSideRef.current = popupSide;
  const hasActiveDownload = activeCount > 0;
  const shouldShowLauncher = launcherVisibility === 'always' || (launcherVisibility === 'downloading' && hasActiveDownload);

  const captureMenu = useCallback((element: HTMLElement | null) => { menuElement.current = element; }, []);

  const applyRegions = useCallback((regions: LauncherInputRegion[]) => {
    const update = regionQueue.current.then(async () => {
      if (disposed.current) return;
      await setLauncherInputRegions(regions);
      lastValidRegions.current = regions;
    });
    // Recover the queue, while propagating this request's failure to native dragging.
    regionQueue.current = update.catch(() => undefined);
    return update;
  }, []);

  const requestMode = useCallback((next: LauncherSurfaceMode) => {
    if (disposed.current) return;
    requestedMode.current = next;
    transitionQueue.current = transitionQueue.current.then(async () => {
      if (disposed.current) return;
      const target = requestedMode.current;
      const layout = target === 'compact'
        ? null
        : await Promise.all([getLauncherPopupPlacement(), getLauncherPopupSide()]);
      if (disposed.current || target !== requestedMode.current) return;
      if (layout) {
        setPopupPlacement(layout[0]);
        setPopupSide(layout[1]);
      }
      setMode(target);
    }).catch(() => undefined);
  }, []);

  const refreshLauncherGeometry = useCallback(async () => {
    const regions = lastValidRegions.current;
    try {
      await applyRegions(regions);
    } catch {
      // The native adapter retains the previous safe regions on failure.
    }
    const [placement, side] = await Promise.all([
      getLauncherPopupPlacement().catch(() => popupPlacementRef.current),
      getLauncherPopupSide().catch(() => popupSideRef.current),
    ]);
    if (disposed.current) return;
    setPopupPlacement(placement);
    setPopupSide(side);
  }, [applyRegions]);

  useLayoutEffect(() => {
    let disposed = false;
    const element = mode === 'context-menu' ? menuElement.current : null;
    const syncInput = () => {
      const rect = element?.getBoundingClientRect();
      const popup = rect && rect.width > 0 && rect.height > 0
        ? { x: rect.x, y: rect.y, width: rect.width, height: rect.height } : null;
      const update = applyRegions(popup ? [launcherIconRegion, popup] : [launcherIconRegion]);
      // Do not keep the whole launcher hidden if a transient WebView child
      // HWND update fails during startup. The Rust side retains the previous
      // safe region and this component retries on every layout/geometry change.
      void update.finally(() => { if (!disposed) setInputReady(true); }).catch(() => undefined);
      return update;
    };
    void syncInput();
    // The menu can change its actual input rectangle as settings expand.
    const observer = element && typeof ResizeObserver !== 'undefined' ? new ResizeObserver(syncInput) : null;
    if (element) observer?.observe(element);
    return () => { disposed = true; observer?.disconnect(); };
  }, [applyRegions, mode, popupPlacement, popupSide]);

  useEffect(() => {
    disposed.current = false;
    return () => { disposed.current = true; };
  }, []);

  useEffect(() => {
    const close = () => requestMode('compact');
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') close(); };
    window.addEventListener('blur', close);
    window.addEventListener('keydown', escape);
    return () => { window.removeEventListener('blur', close); window.removeEventListener('keydown', escape); };
  }, [requestMode]);

  useEffect(() => {
    void anchorLauncherToWorkArea()
      .then(async () => { if (!disposed.current) await refreshLauncherGeometry(); })
      .catch(() => undefined)
      .finally(() => { if (!disposed.current) setAnchored(true); });
  }, [refreshLauncherGeometry]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let cleanedUp = false;
    const unlisten: (() => void)[] = [];
    const refresh = () => { if (!launcherDragging.current) void refreshLauncherGeometry(); };
    const launcher = getCurrentWindow();
    void launcher.onMoved(refresh).then((stop) => {
      if (cleanedUp) stop(); else unlisten.push(stop);
    }).catch(() => undefined);
    void launcher.onScaleChanged(refresh).then((stop) => {
      if (cleanedUp) stop(); else unlisten.push(stop);
    }).catch(() => undefined);
    return () => {
      cleanedUp = true;
      unlisten.forEach((stop) => stop());
    };
  }, [refreshLauncherGeometry]);

  useEffect(() => {
    if (!isTauriRuntime()) { setSettingsReady(true); return; }
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void getSettings().then((settings) => {
      if (!disposed) setLauncherVisibility(settings.launcherVisibility);
    }).catch(() => undefined).finally(() => { if (!disposed) setSettingsReady(true); });
    listen<LauncherVisibility>('launcher-visibility-updated', (event) => {
      if (!disposed) setLauncherVisibility(event.payload);
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; }).catch(() => undefined);
    return () => { disposed = true; unlisten?.(); };
  }, []);

  useEffect(() => {
    if (!settingsReady || !anchored || !inputReady) return;
    void setLauncherWindowVisible(shouldShowLauncher).catch(() => undefined);
  }, [anchored, inputReady, settingsReady, shouldShowLauncher]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlistenDownloads: (() => void) | undefined;
    const applyStat = (stat: DownloadGlobalStat) => {
      setActiveCount(stat.num_active);
      setDownloadSpeed(stat.download_speed);
    };
    subscribeToDownloadEvents({
      onTask: () => undefined,
      onGlobalStat: (stat) => {
        statEventEpoch.current += 1;
        if (!disposed) applyStat(stat);
      }
    }).then(async (stop) => {
      if (disposed) { stop(); return; }
      unlistenDownloads = stop;
      const statEpoch = statEventEpoch.current;
      void getDownloadGlobalStat().then((stat) => {
        if (!disposed && statEventEpoch.current === statEpoch) applyStat(stat);
      }).catch(() => undefined);
    }).catch(() => undefined);
    return () => { disposed = true; unlistenDownloads?.(); };
  }, []);

  function openContextMenu() {
    // WebKitGTK can deliver its native context-menu signal before or after
    // the DOM context-menu event. Treat both deliveries as one open request
    // instead of allowing a duplicate to close the menu again.
    if (requestedMode.current !== 'context-menu') requestMode('context-menu');
    void getLauncherManagerVisible().then(setManagerVisible).catch(() => undefined);
    // Records menu usage only; this command never changes native visibility.
    void showLauncherMenu().catch(() => undefined);
  }

  useEffect(() => {
    // Linux forwards WebKitGTK's native context-menu signal here. Keeping the
    // state transition in React means the displayed menu and X11 input region
    // always come from the same component.
    const openNativeContextMenu = () => openContextMenu();
    window.addEventListener('versadown:launcher-context-menu', openNativeContextMenu);
    return () => window.removeEventListener('versadown:launcher-context-menu', openNativeContextMenu);
  }, []);

  function closeContextMenu() {
    requestMode('compact');
  }

  function closeContextMenuFromLauncher() {
    if (requestedMode.current === 'context-menu') requestMode('compact');
  }

  function queueLauncherDragMove(pointer: LauncherPointerPosition) {
    dragPendingPointer.current = pointer;
    if (!dragSession.current || dragMoveInFlight.current) return;
    dragMoveInFlight.current = true;
    const pump = async () => {
      while (launcherDragging.current && dragSession.current) {
        const next = dragPendingPointer.current;
        dragPendingPointer.current = null;
        if (!next) return;
        await moveLauncherPointerDrag(dragSession.current, next);
      }
    };
    const completion = pump().catch(() => undefined).finally(() => {
      dragMoveInFlight.current = false;
      if (launcherDragging.current && dragPendingPointer.current) queueLauncherDragMove(dragPendingPointer.current);
    });
    dragMoveCompletion.current = completion;
  }

  async function beginLauncherDrag(pointer: LauncherPointerPosition, grabOffset?: LauncherPointerPosition) {
    if (disposed.current || launcherDragging.current) return;
    launcherDragging.current = true;
    nativeDragInFlight.current = true;
    dragEndPointer.current = null;
    requestedMode.current = 'compact';
    setMode('compact');
    try {
      const handled = await runNativeLauncherDrag(grabOffset);
      nativeDragInFlight.current = false;
      if (handled) {
        launcherDragging.current = false;
        dragPendingPointer.current = null;
        if (!disposed.current) await refreshLauncherGeometry();
        return;
      }
      dragSession.current = await beginLauncherPointerDrag(pointer, grabOffset);
      if (disposed.current || !launcherDragging.current) return;
      const pendingPointer = dragPendingPointer.current;
      if (pendingPointer) queueLauncherDragMove(pendingPointer);
      // Linux keeps its X11 input shape expanded only until the native button
      // release. Applying the compact shape here would stop pointer events
      // before this captured browser drag reaches the screen edge.
      if (dragEndPointer.current) finishLauncherDrag(dragEndPointer.current);
    } catch {
      nativeDragInFlight.current = false;
      launcherDragging.current = false;
      dragSession.current = null;
      dragPendingPointer.current = null;
      // Never begin pointer-driven dragging after the compact-region update fails.
    }
  }

  function moveLauncherDrag(pointer: LauncherPointerPosition) {
    if (nativeDragInFlight.current) { dragPendingPointer.current = pointer; return; }
    if (launcherDragging.current) queueLauncherDragMove(pointer);
  }

  function finishLauncherDrag(pointer: LauncherPointerPosition) {
    if (!launcherDragging.current) return;
    // Windows owns both the final position and mouse-release detection. A
    // WebView pointercancel must not race it with a second position update.
    if (nativeDragInFlight.current) { dragEndPointer.current = pointer; return; }
    queueLauncherDragMove(pointer);
    // A fast pointer-up can arrive while the safe icon-only region is still
    // being applied.  Wait for that start sequence to install its session,
    // then flush the final captured pointer position before refreshing.
    void dragStartQueue.current.then(async () => {
      await dragMoveCompletion.current;
      if (dragPendingPointer.current) {
        queueLauncherDragMove(dragPendingPointer.current);
        await dragMoveCompletion.current;
      }
    }).finally(async () => {
      launcherDragging.current = false;
      dragSession.current = null;
      dragPendingPointer.current = null;
      if (!disposed.current) await refreshLauncherGeometry();
    });
  }

  function runContextAction(action: LauncherMenuAction) {
    void runLauncherMenuAction(`launcher.${action}`).then(async () => {
      if (action === 'hide_manager') setManagerVisible(await getLauncherManagerVisible().catch(() => managerVisible));
    }).catch(() => undefined);
  }

  function updateLauncherVisibility(nextVisibility: LauncherVisibility) {
    void getSettings()
      .then((settings) => updateSettings({ ...settings, launcherVisibility: nextVisibility }))
      .then((settings) => setLauncherVisibility(settings.launcherVisibility))
      .catch(() => undefined);
  }

  async function stageLauncherTorrent(file: File): Promise<string> {
    return stageDroppedTorrent(file.name, new Uint8Array(await file.arrayBuffer()));
  }

  return (
    <main className={`launcher-shell ${mode === 'context-menu' ? 'launcher-menu-open' : ''} ${popupPlacement === 'below' ? 'launcher-popup-below' : ''} ${popupSide === 'right' ? 'launcher-popup-right' : ''}`} aria-label={t('launcher.windowLabel')}>
      <FloatingLauncher
        onScan={scanDownloadInputs}
        onDroppedInput={openLauncherNewDownload}
        onStageDroppedTorrent={stageLauncherTorrent}
        onActivateTaskList={async () => { setManagerVisible(await toggleLauncherManager().catch(() => managerVisible)); }}
        onDragStart={(pointer, grabOffset) => { dragStartQueue.current = beginLauncherDrag(pointer, grabOffset); }}
        onDragMove={moveLauncherDrag}
        onDragEnd={finishLauncherDrag}
        onPrimaryClick={closeContextMenuFromLauncher}
        onContextMenu={openContextMenu}
      />
      {mode === 'context-menu' && <LauncherContextMenu onElement={captureMenu} downloadSpeed={formatSpeed(downloadSpeed)} managerVisible={managerVisible} visibility={launcherVisibility} onClose={closeContextMenu} onAction={runContextAction} onVisibilityChange={updateLauncherVisibility} />}
    </main>
  );
}
