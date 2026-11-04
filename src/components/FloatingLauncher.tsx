import { DragEvent as ReactDragEvent, MouseEvent as ReactMouseEvent, PointerEvent, useRef, useState, useSyncExternalStore } from 'react';
import darkMintIcon from '../assets/launcher/dark_mint.svg';
import mintWhiteIcon from '../assets/launcher/mint_white.svg';
import { getResolvedTheme, subscribeTheme } from '../lib/theme';
import type { ScanDownloadResult } from '../lib/types';
import { t } from '../i18n';

export type LauncherPointerPosition = { x: number; y: number };

interface FloatingLauncherProps {
  onScan?(input: string): Promise<ScanDownloadResult[]>;
  onActivateTaskList?(): void | Promise<void>;
  onDroppedInput?(input: string): void | Promise<void>;
  onStageDroppedTorrent?(file: File): Promise<string>;
  onDragStart?(pointer: LauncherPointerPosition, grabOffset?: LauncherPointerPosition): void;
  onDragMove?(pointer: LauncherPointerPosition): void;
  onDragEnd?(pointer: LauncherPointerPosition): void;
  onPrimaryClick?(): void;
  onContextMenu?(): void;
}

const dragThreshold = 5;

function screenPointerPosition(event: { screenX: number; screenY: number }): LauncherPointerPosition {
  // clientX/clientY are relative to this WebView. As the launcher window
  // moves, those values change even when the physical cursor is stationary,
  // creating a feedback loop that makes the icon oscillate. Screen-space
  // coordinates remain stable throughout the captured drag.
  return { x: event.screenX, y: event.screenY };
}

function draggedText(dataTransfer: DataTransfer): string {
  return [...new Set(['text/uri-list', 'text/plain']
    .map((type) => dataTransfer.getData(type).trim())
    .filter(Boolean))]
    .join('\n');
}

function isTorrentFile(file: File): boolean {
  return file.name.trim().toLocaleLowerCase().endsWith('.torrent');
}

function LauncherMark({ theme }: { theme: 'light' | 'dark' }) {
  // WebKitGTK composites an external SVG resource correctly in a transparent
  // window, whereas this complex inline SVG (gradients + clip paths) is not
  // painted there. These are the same themed assets as the visible main mark.
  return <img className="launcher-mark" src={theme === 'dark' ? darkMintIcon : mintWhiteIcon} alt="" aria-hidden="true" width={64} height={64} draggable={false} />;
}

export function FloatingLauncher({ onScan, onActivateTaskList, onDroppedInput, onStageDroppedTorrent, onDragStart, onDragMove, onDragEnd, onPrimaryClick, onContextMenu }: FloatingLauncherProps) {
  const dragOrigin = useRef<{ x: number; y: number } | null>(null);
  const dragGrabOffset = useRef<LauncherPointerPosition>();
  const dragPointerId = useRef<number | null>(null);
  const dragging = useRef(false);
  const suppressClick = useRef(false);
  const resolvedTheme = useSyncExternalStore(subscribeTheme, getResolvedTheme, getResolvedTheme);
  const dropInFlight = useRef(false);
  const [dropTarget, setDropTarget] = useState(false);

  function beginPointer(event: PointerEvent<HTMLDivElement>) {
    if (event.button !== 0) return;
    dragOrigin.current = screenPointerPosition(event);
    dragGrabOffset.current = { x: event.clientX, y: event.clientY };
    dragPointerId.current = event.pointerId;
    dragging.current = false;
    suppressClick.current = false;
    try { event.currentTarget.setPointerCapture(event.pointerId); } catch { /* synthetic events have no native pointer */ }
  }

  function movePointer(event: PointerEvent<HTMLDivElement>) {
    const origin = dragOrigin.current;
    if (!origin || dragPointerId.current !== event.pointerId) return;
    const pointer = screenPointerPosition(event);
    if (!dragging.current) {
      if (Math.hypot(pointer.x - origin.x, pointer.y - origin.y) < dragThreshold) return;
      dragging.current = true;
      suppressClick.current = true;
      onDragStart?.(origin, dragGrabOffset.current);
    }
    onDragMove?.(pointer);
  }

  function endPointer(event: PointerEvent<HTMLDivElement>) {
    if (dragPointerId.current !== event.pointerId) return;
    if (dragging.current) onDragEnd?.(screenPointerPosition(event));
    dragging.current = false;
    dragOrigin.current = null;
    dragPointerId.current = null;
    try { event.currentTarget.releasePointerCapture(event.pointerId); } catch { /* pointer may already be released */ }
  }

  // WebView2 dispatches Pointer Events before compatibility mouse events, so
  // this fallback remains dormant there once pointer capture is active.  It
  // keeps mouse-only runtimes (including the lightweight DOM test environment)
  // functional without weakening the captured production path.
  function beginMouseFallback(event: ReactMouseEvent<HTMLDivElement>) {
    if (dragPointerId.current !== null || event.button !== 0) return;
    dragOrigin.current = screenPointerPosition(event);
    dragGrabOffset.current = { x: event.clientX, y: event.clientY };
    dragPointerId.current = -1;
    dragging.current = false;
    suppressClick.current = false;
  }

  function moveMouseFallback(event: ReactMouseEvent<HTMLDivElement>) {
    if (dragPointerId.current !== -1) return;
    const origin = dragOrigin.current;
    if (!origin) return;
    const pointer = screenPointerPosition(event);
    if (!dragging.current) {
      if (Math.hypot(pointer.x - origin.x, pointer.y - origin.y) < dragThreshold) return;
      dragging.current = true;
      suppressClick.current = true;
      onDragStart?.(origin, dragGrabOffset.current);
    }
    onDragMove?.(pointer);
  }

  function endMouseFallback(event: ReactMouseEvent<HTMLDivElement>) {
    if (dragPointerId.current !== -1) return;
    if (dragging.current) onDragEnd?.(screenPointerPosition(event));
    dragging.current = false;
    dragOrigin.current = null;
    dragPointerId.current = null;
  }

  function clearSuppressedClick() {
    dragOrigin.current = null;
    if (suppressClick.current) {
      suppressClick.current = false;
      return;
    }
    onPrimaryClick?.();
  }

  function activateTaskList() {
    dragOrigin.current = null;
    if (suppressClick.current) return;
    void onActivateTaskList?.();
  }

  function beginDrop(event: ReactDragEvent<HTMLDivElement>) {
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
    setDropTarget(true);
  }

  function leaveDrop(event: ReactDragEvent<HTMLDivElement>) {
    if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDropTarget(false);
  }

  async function acceptDrop(event: ReactDragEvent<HTMLDivElement>) {
    event.preventDefault();
    event.stopPropagation();
    setDropTarget(false);
    if (dropInFlight.current) return;
    dropInFlight.current = true;
    try {
      const inputs: string[] = [];
      const torrents = Array.from(event.dataTransfer.files).filter(isTorrentFile);
      if (torrents.length && onStageDroppedTorrent) {
        inputs.push(...await Promise.all(torrents.map((file) => onStageDroppedTorrent(file))));
      }
      const text = draggedText(event.dataTransfer);
      // A Finder file drag also carries text/uri-list. Once file contents
      // are staged, that URL is another representation of the same input.
      if (!torrents.length && text && onScan) {
        const scanned = await onScan(text);
        inputs.push(...scanned.filter((item) => item.valid).map((item) => item.input));
      }
      const validInputs = [...new Set(inputs.map((input) => input.trim()).filter(Boolean))];
      if (!validInputs.length || !onDroppedInput) return;
      await onDroppedInput(validInputs.join('\n'));
    } finally {
      dropInFlight.current = false;
    }
  }

  return (
    <>
      <div
        className={`launcher-interaction-target${dropTarget ? ' launcher-drop-target' : ''}`}
        onPointerDown={beginPointer}
        onPointerMove={movePointer}
        onPointerUp={endPointer}
        onPointerCancel={endPointer}
        onMouseDown={beginMouseFallback}
        onMouseMove={moveMouseFallback}
        onMouseUp={endMouseFallback}
        onDragEnter={beginDrop}
        onDragOver={beginDrop}
        onDragLeave={leaveDrop}
        onDrop={(event) => { void acceptDrop(event); }}
        onClick={clearSuppressedClick}
        onDoubleClick={activateTaskList}
        onContextMenu={(event) => {
          event.preventDefault();
          dragOrigin.current = null;
          onContextMenu?.();
        }}
      >
        <button className="launcher-trigger" data-size="64" data-icon-variant={resolvedTheme} aria-label={t('launcher.openDownloadInput')}>
          <LauncherMark theme={resolvedTheme} />
        </button>
      </div>
    </>
  );
}
