import { fireEvent, render, screen } from '@testing-library/react';
import { readFileSync } from 'node:fs';
import { useState } from 'react';
import { vi } from 'vitest';
import userEvent from '@testing-library/user-event';
import { DownloadQueue, taskContextMenuPosition } from './DownloadQueue';

it('reorders all selected top-level tasks with pointer events when native HTML dragging is unavailable', () => {
  class TestPointerEvent extends MouseEvent {
    pointerId: number;
    constructor(type: string, init: MouseEventInit & { pointerId?: number } = {}) {
      super(type, init);
      this.pointerId = init.pointerId ?? 0;
    }
  }
  const originalPointerEvent = window.PointerEvent;
  Object.defineProperty(window, 'PointerEvent', { configurable: true, value: TestPointerEvent });
  const onReorder = vi.fn();
  render(<DownloadQueue tasks={[
    { id: 'three', name: 'three.zip', kind: 'file', status: 'paused', progress: 0 },
    { id: 'one', name: 'one.zip', kind: 'file', status: 'paused', progress: 0 },
    { id: 'two', name: 'two.zip', kind: 'file', status: 'paused', progress: 0 },
  ]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} onReorder={onReorder} />);

  fireEvent.click(screen.getByText('one.zip'));
  fireEvent.click(screen.getByText('two.zip'), { ctrlKey: true });
  const first = screen.getByText('one.zip').closest('.task-row')!;
  const third = screen.getByText('three.zip').closest('.task-row')!;
  const firstGroup = first.closest('.task-group')!;
  vi.spyOn(firstGroup, 'getBoundingClientRect').mockReturnValue({ top: 20, bottom: 92, height: 72, left: 10, right: 370, width: 360, x: 10, y: 20, toJSON: () => ({}) });
  vi.spyOn(third, 'getBoundingClientRect').mockReturnValue({ top: 0, bottom: 40, height: 40, left: 0, right: 100, width: 100, x: 0, y: 0, toJSON: () => ({}) });
  const originalElementFromPoint = document.elementFromPoint;
  Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: vi.fn(() => third) });
  fireEvent.pointerDown(first, { pointerId: 7, button: 0, clientX: 10, clientY: 50 });
  fireEvent.pointerMove(first, { pointerId: 7, clientX: 80, clientY: 5 });

  expect(document.querySelector('.task-drag-preview')).toHaveStyle({ left: '80px', top: '-25px' });
  expect([...document.querySelectorAll<HTMLElement>('.download-queue > .task-group .task-row')].map((row) => row.dataset.taskId)).toEqual(['one', 'two', 'three']);

  fireEvent.pointerUp(first, { pointerId: 7, clientX: 80, clientY: 5 });
  if (originalElementFromPoint) Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: originalElementFromPoint });
  else delete (document as Document & { elementFromPoint?: typeof document.elementFromPoint }).elementFromPoint;
  if (originalPointerEvent) Object.defineProperty(window, 'PointerEvent', { configurable: true, value: originalPointerEvent });
  else delete (window as Window & { PointerEvent?: typeof window.PointerEvent }).PointerEvent;

  expect(onReorder).toHaveBeenCalledWith(['one', 'two', 'three']);
});

it('selects all top-level tasks with Ctrl+A and reuses their batch menu', () => {
  const onBatchCommand = vi.fn();
  render(<DownloadQueue tasks={[
    { id: 'one', name: 'one.zip', kind: 'file', status: 'paused', progress: 0 },
    { id: 'two', name: 'two.zip', kind: 'file', status: 'paused', progress: 0 },
  ]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} onBatchCommand={onBatchCommand} />);
  expect(fireEvent.keyDown(document, { key: 'a', ctrlKey: true })).toBe(false);
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(2);
  fireEvent.contextMenu(screen.getByText('one.zip'));
  fireEvent.click(screen.getByRole('menuitem', { name: /开始/ }));
  expect(onBatchCommand).toHaveBeenCalledWith(['one', 'two'], 'resume');
});

it('does not hijack Ctrl+A while editing or when a dialog is open', () => {
  const { rerender } = render(<><input aria-label="edit" /><div contentEditable data-testid="editable" />
    <DownloadQueue tasks={[{ id: 'one', name: 'one.zip', kind: 'file', status: 'paused', progress: 0 }]}
      selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} /></>);
  expect(fireEvent.keyDown(screen.getByLabelText('edit'), { key: 'a', ctrlKey: true })).toBe(true);
  expect(fireEvent.keyDown(screen.getByTestId('editable'), { key: 'a', ctrlKey: true })).toBe(true);
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(0);
  rerender(<><div role="dialog" aria-modal="true">Confirm</div>
    <DownloadQueue tasks={[{ id: 'one', name: 'one.zip', kind: 'file', status: 'paused', progress: 0 }]}
      selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} /></>);
  expect(fireEvent.keyDown(document, { key: 'a', ctrlKey: true })).toBe(true);
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(0);
});

it('shows downloading peer counts after the peer-to-peer task type', () => {
  render(<DownloadQueue tasks={[
    { id: 'magnet', name: 'movie.mkv', kind: 'magnet', status: 'downloading', progress: 25, connections: 3, discovered_nodes: 8 },
    { id: 'torrent', name: 'bundle', kind: 'torrent', status: 'downloading', progress: 25, connections: 1, discovered_nodes: 4 },
    { id: 'ed2k', name: 'archive.zip', kind: 'ed2k', status: 'downloading', progress: 25, connections: 2 },
    { id: 'paused', name: 'paused.torrent', kind: 'torrent', status: 'paused', progress: 25, connections: 6, discovered_nodes: 9 },
    { id: 'file', name: 'file.iso', kind: 'file', status: 'downloading', progress: 25, connections: 99 },
  ]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);

  const magnetPeers = screen.getByText('(3/8)');
  const torrentPeers = screen.getByText('(1/4)');
  const ed2kPeers = screen.getByText('(2/0)');
  expect(magnetPeers.closest('.task-subtitle')).toHaveTextContent('磁力任务(3/8)');
  expect(torrentPeers.closest('.task-subtitle')).toHaveTextContent('BT 种子(1/4)');
  expect(ed2kPeers.closest('.task-subtitle')).toHaveTextContent('ED2K 下载(2/0)');
  expect(magnetPeers.closest('.task-copy')).toBeNull();
  expect(screen.queryByText('(6/9)')).not.toBeInTheDocument();
  expect(screen.queryByText('(99/0)')).not.toBeInTheDocument();
});

it('uses one queued control with the regular task menu until aria2 promotes the task', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const { rerender } = render(<DownloadQueue tasks={[
    { id: 'queued-task', name: 'waiting.zip', kind: 'file', status: 'queued', progress: 0 },
  ]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  const queued = screen.getByRole('button', { name: 'waiting.zip 排队中' });
  expect(queued).toHaveTextContent('排队中');
  expect(queued).toBeEnabled();
  expect(queued).toHaveAttribute('aria-haspopup', 'menu');
  expect(screen.queryByRole('button', { name: '更多操作 waiting.zip' })).not.toBeInTheDocument();
  expect(queued.closest('.task-actions')?.querySelectorAll('button')).toHaveLength(1);
  await user.click(queued);
  expect(screen.getByRole('menu', { name: 'waiting.zip任务菜单' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '打开下载目录' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '删除任务' })).toBeVisible();

  rerender(<DownloadQueue tasks={[
    { id: 'queued-task', name: 'waiting.zip', kind: 'file', status: 'downloading', progress: 0 },
  ]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  expect(screen.getByRole('button', { name: '暂停 waiting.zip' })).toBeEnabled();
  expect(screen.getByRole('button', { name: '更多操作 waiting.zip' })).toBeVisible();
  expect(screen.queryByRole('button', { name: 'waiting.zip 排队中' })).not.toBeInTheDocument();
});

it('shows a sharing marker only for completed peer-to-peer tasks that are still uploading', () => {
  render(<DownloadQueue tasks={[
    { id: 'sharing', name: 'sharing.mkv', kind: 'magnet', status: 'completed', progress: 100, completed_at: new Date(2026, 8, 11, 11, 2).getTime() / 1000, p2p_sharing: true },
    { id: 'finished', name: 'finished.zip', kind: 'ed2k', status: 'completed', progress: 100, completed_at: new Date(2026, 8, 11, 11, 2).getTime() / 1000, p2p_sharing: false },
  ]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);

  expect(screen.getByText('P2P 分享中')).toBeVisible();
  expect(screen.getAllByText(/2026-09-11 11:02/)).toHaveLength(2);
  expect(screen.getAllByText('P2P 分享中')).toHaveLength(1);
});

const folderTask = {
  id: 'folder-1', name: 'Season 1', kind: 'folder', status: 'downloading', progress: 42,
  children: [{ id: 'file-1', name: 'episode-01.mkv', kind: 'file', status: 'downloading', progress: 50, size: '8 MB / 161.2 MB' }]
} as const;

it('shows live speed and downloaded bytes for expanded directory and file rows', async () => {
  const user = userEvent.setup();
  const { container } = render(<DownloadQueue tasks={[{
    id: 'metrics-root', name: '传输目录', kind: 'folder', status: 'downloading', progress: 25,
    children: [{
      id: 'metrics-folder', name: '子目录', kind: 'folder', status: 'downloading', progress: 25,
      total_length: 2097152, completed_length: 524288, download_speed: 262144,
      children: [{ id: 'metrics-file', name: 'clip.mp4', kind: 'cloud', status: 'downloading', progress: 25,
        total_length: 2097152, completed_length: 524288, download_speed: 131072 }],
    }],
  }]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
  await user.click(screen.getByRole('button', { name: '展开 传输目录' }));
  await user.click(screen.getByRole('button', { name: '展开 子目录' }));
  const rows = container.querySelectorAll('.child-row');
  expect(rows[0].querySelector('.child-transfer-size')).toHaveTextContent('512 KB / 2 MB');
  expect(rows[1].querySelector('.child-transfer-size')).toHaveTextContent('512 KB / 2 MB');
  expect(rows[0].querySelector('.child-speed')).toHaveTextContent('256 KB/s');
  expect(rows[1].querySelector('.child-speed')).toHaveTextContent('128 KB/s');
});

it('preserves completed file sizes and hides stale speed for paused children', async () => {
  const user = userEvent.setup();
  const { container } = render(<DownloadQueue tasks={[{
    id: 'sizes-root', name: '大小目录', kind: 'folder', status: 'downloading', progress: 50,
    children: [
      { id: 'paused-size', name: 'paused.bin', kind: 'cloud', status: 'paused', progress: 25,
        total_length: 2097152, completed_length: 524288, download_speed: 131072 },
      { id: 'unknown-size', name: 'unknown.bin', kind: 'cloud', status: 'downloading', progress: 0,
        completed_length: 131072, download_speed: 0 },
      { id: 'complete-size', name: 'complete.bin', kind: 'cloud', status: 'completed', progress: 100,
        total_length: 4194304, completed_length: 32768, download_speed: 131072 },
    ],
  }]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
  await user.click(screen.getByRole('button', { name: '展开 大小目录' }));
  const rows = container.querySelectorAll('.child-row');
  expect(rows[0].querySelector('.child-transfer-size')).toHaveTextContent('512 KB / 2 MB');
  expect(rows[0].querySelector('.child-speed')).toBeNull();
  expect(rows[1].querySelector('.child-transfer-size')).toHaveTextContent('128 KB / 未知');
  expect(rows[1].querySelector('.child-speed')).toHaveTextContent('0 B/s');
  expect(rows[2].querySelector('.child-transfer-size')).toHaveTextContent('4 MB');
  expect(rows[2].querySelector('.child-speed')).toBeNull();
});

it('places multi-file controls and child progress in stable aligned columns', async () => {
  const user = userEvent.setup();
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const { container } = render(<DownloadQueue tasks={[folderTask]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
    await user.click(screen.getByRole('button', { name: '展开 Season 1' }));

    const expandSlot = container.querySelector('.expand-slot');
    const childRow = container.querySelector('.child-row');
    const childMarker = container.querySelector('.child-name i');
    const childProgress = container.querySelector('.child-progress');
    const childProgressValue = container.querySelector('.child-progress-value');
    const childTransferSize = container.querySelector('.child-transfer-size');
    const childAction = container.querySelector('.child-actions button');

    expect(getComputedStyle(expandSlot!).gridArea).toBe('auto');
    expect(getComputedStyle(expandSlot!).left).toBe('0px');
    expect(getComputedStyle(expandSlot!).width).toBe('22px');
    expect(getComputedStyle(childRow!).paddingLeft).toBe('21px');
    expect(getComputedStyle(childRow!).paddingTop).toBe('6px');
    expect(getComputedStyle(childRow!).paddingBottom).toBe('6px');
    expect(getComputedStyle(childRow!).fontSize).toBe('13px');
    expect(childMarker).toBeNull();
    expect(getComputedStyle(childRow!).gridTemplateColumns).toBe('minmax(0,1fr) minmax(140px,220px) 44px 120px 56px');
    expect(getComputedStyle(childRow!).paddingRight).toBe('24px');
    expect(getComputedStyle(childProgress!).justifySelf).toBe('end');
    expect(childProgress!.querySelector('small')).toBeNull();
    expect(childProgressValue).toHaveTextContent('50%');
    expect(childTransferSize).toHaveTextContent('8 MB / 161.2 MB');
    expect(getComputedStyle(childProgressValue!).textAlign).toBe('right');
    expect(getComputedStyle(childTransferSize!).textAlign).toBe('right');
    expect(getComputedStyle(childAction!).width).toBe('26px');
    expect(getComputedStyle(childAction!).height).toBe('26px');
  } finally {
    stylesheet.remove();
  }
});

it('positions a bottom task menu fully inside the viewport', () => {
  expect(taskContextMenuPosition({
    x: 980,
    y: 386,
    menuWidth: 224,
    menuHeight: 344,
    viewportWidth: 1560,
    viewportHeight: 460,
  })).toEqual({ left: 980, top: 108 });
});

it('keeps a folder as one row until the user expands it', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue tasks={[folderTask]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);
  expect(screen.queryByText('episode-01.mkv')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '展开 Season 1' }));
  expect(screen.getByText('episode-01.mkv')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '暂停 episode-01.mkv' }));
  expect(onCommand).toHaveBeenCalledWith('file-1', 'pause');
});

it('skips offscreen rendering for collapsed task groups but restores full layout when expanded', async () => {
  const user = userEvent.setup();
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const { container } = render(<DownloadQueue tasks={[folderTask]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
    const taskGroup = container.querySelector('.task-group')!;

    expect(getComputedStyle(taskGroup).contentVisibility).toBe('auto');

    await user.click(screen.getByRole('button', { name: '展开 Season 1' }));
    expect(taskGroup).toHaveClass('is-expanded');
    expect(getComputedStyle(taskGroup).contentVisibility).not.toBe('auto');
  } finally {
    stylesheet.remove();
  }
});

it('reveals nested cloud directory files only after each directory is expanded', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue tasks={[{
    id: 'season-1', name: 'Season 1', kind: 'folder', status: 'downloading', progress: 40,
    children: [
      {
        id: 'extras', name: 'Extras', kind: 'folder', status: 'downloading', progress: 25,
        children: [{ id: 'behind', name: 'behind.mkv', kind: 'cloud', status: 'downloading', progress: 50, size: '50 MB / 100 MB' }],
      },
      { id: 'failed-link', name: 'unavailable.mkv', kind: 'cloud', status: 'failed', progress: 0, error: 'link generation failed' },
    ],
  }]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  expect(screen.queryByText('Extras')).not.toBeInTheDocument();
  expect(screen.queryByText('behind.mkv')).not.toBeInTheDocument();
  expect(screen.queryByText('link generation failed')).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: '展开 Season 1' }));
  expect(screen.getByText('Extras')).toBeVisible();
  expect(screen.getByText('link generation failed')).toBeVisible();
  expect(screen.queryByText('behind.mkv')).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: '展开 Extras' }));
  expect(screen.getByText('behind.mkv')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '暂停 behind.mkv' }));
  expect(onCommand).toHaveBeenCalledWith('behind', 'pause');
});

it('shows an active cloud directory with the standard transfer metrics', () => {
  render(<DownloadQueue
    tasks={[{
      id: 'shared-directory', name: 'YM-JFSMBYHS', kind: 'folder', status: 'downloading', progress: 9,
      total_length: 178 * 1024 * 1024, completed_length: Math.round(15.8 * 1024 * 1024),
      download_speed: Math.round(313.5 * 1024), eta_seconds: 8 * 60 + 49,
      children: [{ id: 'episode', name: 'episode.mp4', kind: 'cloud', status: 'downloading', progress: 9 }],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByText('15.8 MB / 178 MB')).toBeVisible();
  expect(screen.queryByText('网盘下载任务')).not.toBeInTheDocument();
  expect(screen.getByText('剩余 8 分 49 秒')).toBeVisible();
  expect(screen.getByText('313.5 KB/s')).toBeVisible();
  expect(screen.queryByText('目录任务 · 1 个文件')).not.toBeInTheDocument();
});

it('places remaining time at the right edge of the task title row', () => {
  render(<DownloadQueue
    tasks={[{
      id: 'stable-live-metrics', name: 'large-file.bin', kind: 'file', status: 'downloading', progress: 34,
      total_length: 500 * 1024 * 1024, completed_length: 170 * 1024 * 1024,
      download_speed: Math.round(6.6 * 1024 * 1024), eta_seconds: 49,
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  const eta = screen.getByText('剩余 49 秒');
  expect(eta).toHaveClass('task-title-eta');
  expect(eta.closest('.task-copy')).not.toBeNull();
  expect(eta.closest('.task-progress')).toBeNull();
  expect(eta.parentElement?.lastElementChild).toBe(eta);
  expect(screen.getByText('6.6 MB/s')).toHaveClass('transfer-speed');
});

it('renders completed and active task rows with the same geometry', () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const { container } = render(<DownloadQueue
      tasks={[
        { id: 'active-row', name: 'active.bin', kind: 'file', status: 'downloading', progress: 40 },
        { id: 'completed-row', name: 'completed.bin', kind: 'file', status: 'completed', progress: 100 },
      ]}
      selectedTaskId={null}
      onSelect={() => {}}
      onCommand={() => {}}
    />);

    const activeRow = container.querySelector<HTMLElement>('[data-task-id="active-row"]')!;
    const completedRow = container.querySelector<HTMLElement>('[data-task-id="completed-row"]')!;
    expect(getComputedStyle(completedRow).minHeight).toBe(getComputedStyle(activeRow).minHeight);
    expect(getComputedStyle(completedRow.querySelector('.kind-icon')! as Element).width)
      .toBe(getComputedStyle(activeRow.querySelector('.kind-icon')! as Element).width);
  } finally {
    stylesheet.remove();
  }
});

it('renders task download speed in bold', () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<DownloadQueue
      tasks={[{
        id: 'bold-speed', name: 'large-file.bin', kind: 'file', status: 'downloading', progress: 34,
        download_speed: Math.round(6.6 * 1024 * 1024),
      }]}
      selectedTaskId={null}
      onSelect={() => {}}
      onCommand={() => {}}
    />);

    expect(getComputedStyle(screen.getByText('6.6 MB/s')).fontWeight).toBe('700');
  } finally {
    stylesheet.remove();
  }
});

it('does not expose expansion for a single-file torrent task', () => {
  render(<DownloadQueue
    tasks={[{
      id: 'single-file-torrent', name: 'movie.mkv', kind: 'torrent', status: 'downloading', progress: 25,
      files: [{ index: 1, path: 'D://Downloads/movie.mkv', length: 100, completed_length: 25, selected: true }],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.queryByRole('button', { name: '展开 movie.mkv' })).not.toBeInTheDocument();
  expect(screen.queryByText('Downloads/movie.mkv')).not.toBeInTheDocument();
});

it('does not turn non-BT file metadata into a file-selection tree', () => {
  const onFileSelection = vi.fn();
  render(<DownloadQueue
    tasks={[{
      id: 'cloud-fallback-files', name: '云盘目录', kind: 'cloud', status: 'downloading', progress: 20,
      files: [
        { index: 1, path: 'first.bin', length: 100, completed_length: 20, selected: true },
        { index: 2, path: 'second.bin', length: 100, completed_length: 10, selected: true },
      ],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
    onFileSelection={onFileSelection}
  />);

  expect(screen.queryByRole('button', { name: '展开 云盘目录' })).not.toBeInTheDocument();
  expect(screen.queryByText('first.bin')).not.toBeInTheDocument();
  expect(onFileSelection).not.toHaveBeenCalled();
});

it('keeps completed multi-file BT tasks inspectable alongside completed cloud folders', async () => {
  const user = userEvent.setup();
  render(<DownloadQueue
    tasks={[
      {
        id: 'completed-torrent-files', name: '完成的 BT 合集', kind: 'torrent', status: 'completed', progress: 100,
        files: [
          { index: 1, path: 'D:/Downloads/one.mkv', length: 100, completed_length: 100, selected: true },
          { index: 2, path: 'D:/Downloads/two.mkv', length: 100, completed_length: 100, selected: true },
        ],
      },
      {
        id: 'completed-cloud-folder', name: '完成的云盘目录', kind: 'folder', status: 'completed', progress: 100,
        children: [{ id: 'completed-cloud-file', name: 'archived.mkv', kind: 'cloud', status: 'completed', progress: 100 }],
      },
    ]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.queryByText('one.mkv')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '展开 完成的 BT 合集' }));
  expect(screen.getByText('one.mkv')).toBeVisible();
  expect(screen.getByText('two.mkv')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '展开 完成的云盘目录' }));
  expect(screen.getByText('archived.mkv')).toBeVisible();
});

it('renders completed cloud-directory media children with full sizes and playback actions', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[{
      id: 'completed-cloud-directory', name: '已完成媒体目录', kind: 'folder', status: 'completed', progress: 100,
      children: [
        {
          id: 'completed-video-child', name: 'episode.mp4', kind: 'cloud', status: 'completed', progress: 100,
          total_length: 1_048_576, completed_length: 512, error: '网盘分享下载失败，请重试或重新解析分享链接',
        },
        {
          id: 'completed-audio-child', name: 'commentary.mp3', kind: 'cloud', status: 'completed', progress: 100,
          total_length: 2_097_152, completed_length: 1_024,
        },
        {
          id: 'completed-file-metadata-child', name: 'cached.mp4', kind: 'cloud', status: 'completed', progress: 100,
          files: [{ index: 1, path: 'D:/Downloads/cached.mp4', length: 3_145_728, completed_length: 3_145_728, selected: true }],
        },
      ],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  await user.click(screen.getByRole('button', { name: '展开 已完成媒体目录' }));

  expect(screen.getByText('1 MB')).toBeVisible();
  expect(screen.getByText('2 MB')).toBeVisible();
  expect(screen.getByText('3 MB')).toBeVisible();
  expect(screen.queryByText('网盘分享下载失败，请重试或重新解析分享链接')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '播放 episode.mp4' }));
  expect(onCommand).toHaveBeenLastCalledWith('completed-video-child', 'play_videos');
  await user.click(screen.getByRole('button', { name: '播放 commentary.mp3' }));
  expect(onCommand).toHaveBeenLastCalledWith('completed-audio-child', 'play_videos');
});

it('shows each completed file time instead of progress in a completed cloud directory', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const { container } = render(<DownloadQueue
    tasks={[{
      id: 'completed-time-directory', name: '已完成归档', kind: 'folder', status: 'completed', progress: 100,
      children: [{
        id: 'completed-time-file', name: 'finale.mp4', kind: 'cloud', status: 'completed', progress: 100,
        completed_at: new Date(2026, 8, 11, 11, 2).getTime() / 1000, total_length: 1_048_576, completed_length: 1_048_576,
      }],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  await user.click(screen.getByRole('button', { name: '展开 已完成归档' }));

  const fileRow = container.querySelector('.child-row')!;
  expect(fileRow).toHaveTextContent('2026-09-11 11:02');
  expect(fileRow).toHaveTextContent('1 MB');
  expect(fileRow.querySelector('.child-progress')).toBeNull();
  expect(fileRow.querySelector('.child-progress-value')).toBeNull();
  await user.click(screen.getByRole('button', { name: '播放 finale.mp4' }));
  expect(onCommand).toHaveBeenLastCalledWith('completed-time-file', 'play_videos');
});

it('keeps completed nested directories on their existing progress display', async () => {
  const user = userEvent.setup();
  const { container } = render(<DownloadQueue
    tasks={[{
      id: 'completed-nested-root', name: '完成根目录', kind: 'folder', status: 'completed', progress: 100,
      children: [{
        id: 'completed-nested-directory', name: '第二季', kind: 'folder', status: 'completed', progress: 100,
        children: [{
          id: 'completed-nested-file', name: 'finale.mp4', kind: 'cloud', status: 'completed', progress: 100,
          completed_at: new Date(2026, 8, 11, 11, 2).getTime() / 1000,
        }],
      }],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  await user.click(screen.getByRole('button', { name: '展开 完成根目录' }));

  const directoryRow = container.querySelector('.child-row')!;
  expect(directoryRow).toHaveTextContent('100%');
  expect(directoryRow.querySelector('.child-progress')).not.toBeNull();
  expect(directoryRow.querySelector('.child-completion-time')).toBeNull();

  await user.click(screen.getByRole('button', { name: '展开 第二季' }));
  expect(screen.getByText('2026-09-11 11:02')).toBeVisible();
});

it('uses file operations instead of transfer status for expanded multi-file tasks and removes local path noise', async () => {
  const user = userEvent.setup();
  const onFileSelection = vi.fn();
  const onFileCancel = vi.fn();
  render(<DownloadQueue
    tasks={[{
      id: 'torrent-files', name: '演示合集', kind: 'torrent', status: 'downloading', progress: 25,
      files: [
        { index: 1, path: 'D://演示合集/episode-01.mkv', length: 1024 * 1024 * 1024, completed_length: 1536 * 1024, selected: true },
        { index: 2, path: 'D://演示合集/readme.txt', length: 10, completed_length: 0, selected: false },
      ],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
    onFileSelection={onFileSelection}
    onFileCancel={onFileCancel}
  />);

  await user.click(screen.getByRole('button', { name: '展开 演示合集' }));

  expect(screen.getByText('episode-01.mkv')).toBeVisible();
  expect(screen.getByText('readme.txt')).toBeVisible();
  expect(screen.queryByText('D://演示合集/episode-01.mkv')).not.toBeInTheDocument();
  expect(screen.queryByText('下载中')).not.toBeInTheDocument();
  expect(screen.getByTitle('1.5 MB / 1 GB')).toBeVisible();
  expect(screen.queryByText('1572864 B / 1073741824 B')).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: '暂停 episode-01.mkv' }));
  expect(onFileSelection).toHaveBeenCalledWith('torrent-files', 1, false);
  await user.click(screen.getByRole('button', { name: '取消 episode-01.mkv 下载' }));
  expect(onFileCancel).toHaveBeenCalledWith('torrent-files', 1);
  expect(screen.getByRole('button', { name: '继续 readme.txt' })).toBeVisible();
});

it('groups multi-level BT file paths into separately expandable directories', async () => {
  const user = userEvent.setup();
  render(<DownloadQueue
    tasks={[{
      id: 'hierarchical-bt', name: 'Black Myth', kind: 'magnet', status: 'downloading', progress: 12,
      files: [
        { index: 1, path: 'D:/Downloads/Black Myth/_extras/readme.txt', length: 10, completed_length: 0, selected: true },
        { index: 2, path: 'D:/Downloads/Black Myth/Engine/Binaries/game.exe', length: 20, completed_length: 0, selected: true },
        { index: 3, path: 'D:/Downloads/Black Myth/b1.exe', length: 30, completed_length: 0, selected: true },
      ],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  await user.click(screen.getByRole('button', { name: '展开 Black Myth' }));
  expect(screen.getByText('_extras')).toBeVisible();
  expect(screen.getByText('Engine')).toBeVisible();
  expect(screen.getByText('b1.exe')).toBeVisible();
  expect(screen.queryByText('readme.txt')).not.toBeInTheDocument();
  expect(screen.queryByText('Binaries')).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: '展开 _extras' }));
  expect(screen.getByText('readme.txt')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '展开 Engine' }));
  expect(screen.getByText('Binaries')).toBeVisible();
  expect(screen.queryByText('game.exe')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '展开 Binaries' }));
  expect(screen.getByText('game.exe')).toBeVisible();
});

it('shows a continue operation for selected files when their parent task is paused', async () => {
  const user = userEvent.setup();
  render(<DownloadQueue
    tasks={[{
      id: 'paused-torrent', name: '暂停合集', kind: 'torrent', status: 'paused', progress: 25,
      files: [
        { index: 1, path: 'D://暂停合集/episode.mkv', length: 100, completed_length: 25, selected: true },
        { index: 2, path: 'D://暂停合集/readme.txt', length: 10, completed_length: 0, selected: true },
      ],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
    onFileSelection={() => {}}
  />);

  await user.click(screen.getByRole('button', { name: '展开 暂停合集' }));
  expect(screen.getByRole('button', { name: '继续 episode.mkv' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '暂停 episode.mkv' })).not.toBeInTheDocument();
});

it('keeps magnet metadata on the compact transfer-metrics row', () => {
  const source = 'magnet:?xt=urn:btih:46A00FD699618BC0319C45F7075C067D564A0819&dn=Lanterns.S01E04.1080p.HMAX.WEBRip.AAC5.1.10bits.x265-Rapta&tr=udp%3A%2F%2Ftracker.example';
  render(<DownloadQueue
    tasks={[{ id: 'magnet-1', name: source, source, kind: 'magnet', status: 'downloading', progress: 0, speed: '0 B/s', connections: 0 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByText('Lanterns.S01E04.1080p.HMAX.WEBRip.AAC5.1.10bits.x265-Rapta')).toBeVisible();
  expect(screen.queryByText(source)).not.toBeInTheDocument();
  const metadata = screen.getByText('磁力任务 · 正在获取文件信息');
  expect(metadata).toBeVisible();
  expect(metadata.closest('.transfer-metrics')).not.toBeNull();
  expect(metadata.closest('.task-copy')).toBeNull();
  expect(metadata.querySelector('.task-metadata-indicator')).toBeVisible();
});

it('shows the shared loading indicator while a restored cloud task waits for login', () => {
  render(<DownloadQueue
    tasks={[{
      id: 'cloud-waiting', name: 'cloud.bin', kind: 'cloud', status: 'paused', progress: 0,
      error: '恢复网盘登录后继续下载', failure_code: 'authorization_pending', reauthorization_required: true,
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  const message = screen.getByText('恢复网盘登录后继续下载');
  expect(message).toBeVisible();
  expect(message.closest('.task-subtitle')).not.toHaveClass('task-error');
  expect(message.querySelector('.task-metadata-indicator')).toBeVisible();
  expect(screen.getByRole('button', { name: '重新授权 cloud.bin' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '继续 cloud.bin' })).not.toBeInTheDocument();
});

it('renders silent login recovery and native SVIP acceleration as non-error status text', () => {
  const { rerender } = render(<DownloadQueue
    tasks={[{
      id: 'cloud-recovering', name: 'cloud.bin', kind: 'cloud', status: 'paused', progress: 38,
      statusMessage: '正在自动恢复网盘登录', failure_code: 'authorization_recovering',
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);
  const recovering = screen.getByText('正在自动恢复网盘登录');
  expect(recovering).not.toHaveClass('task-error');
  expect(recovering.querySelector('.task-metadata-indicator')).toBeVisible();
  expect(screen.getByText('登录恢复中')).toBeVisible();

  rerender(<DownloadQueue
    tasks={[{
      id: 'cloud-recovering', name: 'cloud.bin', source: 'cloud://xunlei/file-1', kind: 'cloud', status: 'downloading', progress: 39,
      statusMessage: '正在使用迅雷svip加速', aria_gid: 'xunlei-native:42', download_speed: 512_000,
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);
  const svip = screen.getByText('迅雷网盘 · SVIP 加速');
  expect(svip).not.toHaveClass('task-error');
  expect(screen.queryByText('正在使用迅雷svip加速')).not.toBeInTheDocument();
});

it('does not show file-information loading after a magnet transfer has started', () => {
  render(<DownloadQueue
    tasks={[{
      id: 'magnet-active', name: '正在下载的视频', kind: 'magnet', status: 'downloading', progress: 8,
      completed_length: 42 * 1024 * 1024, total_length: 502.5 * 1024 * 1024,
      download_speed: 410_500, connections: 2,
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByText('磁力任务')).toBeVisible();
  expect(screen.queryByText(/正在获取文件信息/)).not.toBeInTheDocument();
});

it('does not render a changing BT file-count summary after metadata is available', () => {
  render(<DownloadQueue
    tasks={[{
      id: 'magnet-files-ready', name: 'Black Myth', kind: 'magnet', status: 'downloading', progress: 8,
      completed_length: 42 * 1024 * 1024, total_length: 142.1 * 1024 * 1024 * 1024,
      files: [
        { index: 1, path: 'Black Myth/b1.exe', length: 100, completed_length: 42, selected: true },
        { index: 2, path: 'Black Myth/Engine/game.exe', length: 100, completed_length: 0, selected: true },
      ],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByText('磁力任务')).toBeVisible();
  expect(screen.queryByText(/个文件/)).not.toBeInTheDocument();
});

it('keeps the live magnet metadata stage in the transfer-metrics row', () => {
  render(<DownloadQueue
    tasks={[{ id: 'magnet-stage', name: '磁力下载', source: 'magnet:?xt=urn:btih:0123456789ABCDEF', kind: 'magnet', status: 'downloading', progress: 0, speed: '0 B/s', connections: 0, statusMessage: '正在获取种子信息，暂无节点连接' }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  const metadata = screen.getByText('正在获取种子信息，暂无节点连接');
  expect(metadata).toBeVisible();
  expect(metadata.closest('.transfer-metrics')).not.toBeNull();
});

it('labels protocol and cloud-provider tasks without exposing raw protocol links', () => {
  render(<DownloadQueue
    tasks={[
      { id: 'ed2k-1', name: 'Example.zip', source: 'ed2k://|file|Example.zip|42|0123456789ABCDEF0123456789ABCDEF|/', kind: 'ed2k', status: 'downloading', progress: 1 },
      { id: 'ftp-1', name: 'file.iso', source: 'ftp://demo:secret@example.test/file.iso', kind: 'ftp', status: 'downloading', progress: 2 },
      { id: 'hls-1', name: 'master.m3u8', source: 'https://media.test/master.m3u8', kind: 'hls', status: 'downloading', progress: 3 },
      { id: 'dash-1', name: 'manifest.mpd', source: 'https://media.test/manifest.mpd', kind: 'dash', status: 'downloading', progress: 4 },
      { id: 'quark-1', name: 'quark.zip', source: 'cloud://quark/file-1', kind: 'cloud', status: 'downloading', progress: 5 },
      { id: 'pikpak-1', name: 'pikpak.zip', source: 'cloud-share://pikpak/share/file-1', kind: 'cloud', status: 'downloading', progress: 6 },
      { id: 'baidu-1', name: 'baidu-folder', source: 'cloud-directory://baidu/group-1', kind: 'folder', status: 'downloading', progress: 7 },
      { id: 'tianyi-1', name: 'tianyi.zip', source: 'cloud://tianyi/file-1', kind: 'cloud', status: 'downloading', progress: 8 },
      { id: 'aliyun-1', name: 'aliyun.zip', source: 'cloud://aliyun/file-1', kind: 'cloud', status: 'downloading', progress: 9 },
      { id: 'xunlei-1', name: 'xunlei.zip', source: 'cloud://xunlei/file-1', kind: 'cloud', status: 'downloading', progress: 10 },
      { id: 'uc-1', name: 'uc.zip', source: 'cloud-directory-item://uc/group-1/file-1', kind: 'cloud', status: 'downloading', progress: 11 },
    ]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByText('ED2K 下载')).toBeVisible();
  expect(screen.getByText('FTP 下载')).toBeVisible();
  expect(screen.getByText('HLS 流媒体下载')).toBeVisible();
  expect(screen.getByText('MPEG-DASH 流媒体下载')).toBeVisible();
  expect(screen.getByText('夸克网盘任务')).toBeVisible();
  expect(screen.getByText('PikPak 网盘任务')).toBeVisible();
  expect(screen.getByText('百度网盘任务')).toBeVisible();
  expect(screen.getByText('天翼云盘任务')).toBeVisible();
  expect(screen.getByText('阿里云盘任务')).toBeVisible();
  expect(screen.getByText('迅雷网盘任务')).toBeVisible();
  expect(screen.getByText('UC 网盘任务')).toBeVisible();
  expect(screen.queryByText(/demo:secret/)).not.toBeInTheDocument();
  expect(screen.queryByText(/cloud-directory:|cloud-share:/)).not.toBeInTheDocument();
});

it('shows transferred bytes, live speed and ETA without exposing peer-count noise', () => {
  render(<DownloadQueue
    tasks={[{
      id: 'transfer-1', name: 'movie.mkv', kind: 'file', status: 'downloading', progress: 25,
      completed_length: 1024 * 1024, total_length: 4 * 1024 * 1024,
      download_speed: 512 * 1024, eta_seconds: 61, connections: 99
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByText('1 MB / 4 MB')).toBeVisible();
  expect(screen.getByText('512 KB/s')).toBeVisible();
  expect(screen.getByText('剩余 1 分 1 秒')).toBeVisible();
  expect(screen.queryByText('99 个连接')).not.toBeInTheDocument();
});

it('replaces paused transfer estimates with a single paused label and no status pill', () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<DownloadQueue
      tasks={[{
        id: 'paused-transfer', name: 'episode.mkv', kind: 'file', status: 'paused', progress: 42,
        completed_length: 42 * 1024 * 1024, total_length: 100 * 1024 * 1024,
        download_speed: 329_700, eta_seconds: 921,
      }]}
      selectedTaskId={null}
      onSelect={() => {}}
      onCommand={() => {}}
    />);

    const paused = screen.getByText('已暂停');
    expect(paused).toBeVisible();
    expect(getComputedStyle(paused).gridColumn).toBe('2');
    expect(getComputedStyle(paused).justifyContent).toBe('flex-end');
    expect(screen.queryByText('剩余 15 分 21 秒')).not.toBeInTheDocument();
    expect(screen.queryByText('322.0 KB/s')).not.toBeInTheDocument();
    expect(screen.queryByText('失败')).not.toBeInTheDocument();
  } finally {
    stylesheet.remove();
  }
});

it('does not render a failure status pill in task rows', () => {
  render(<DownloadQueue
    tasks={[{ id: 'failed-transfer', name: 'archive.zip', kind: 'file', status: 'failed', progress: 30, error: '服务器拒绝连接' }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByText('服务器拒绝连接')).toBeVisible();
  expect(screen.queryByText('失败')).not.toBeInTheDocument();
});

it('offers reauthorization instead of a normal retry for a cloud task blocked by verification', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const task = {
    id: 'reauthorize-cloud-task',
    name: 'blocked.zip',
    kind: 'cloud',
    status: 'failed',
    progress: 0,
    source: 'cloud://baidu/blocked-file',
    error: '网盘要求完成官方操作验证后重试',
    reauthorization_required: true,
  } as unknown as import('../lib/types').QueueTask;
  render(<DownloadQueue tasks={[task]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  expect(screen.getByRole('button', { name: '重新授权 blocked.zip' })).toBeVisible();
  expect(screen.getByText('重新授权')).toBeVisible();
  expect(screen.queryByRole('button', { name: '重新下载 blocked.zip' })).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '更多操作 blocked.zip' }));
  expect(screen.getByRole('menuitem', { name: '重新授权' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '重新下载' })).not.toBeInTheDocument();

  await user.click(screen.getByRole('menuitem', { name: '重新授权' }));
  expect(onCommand).toHaveBeenCalledWith('reauthorize-cloud-task', 'reauthorize');
});

it('opens the directory when a completed task mixes video and other files', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const completedTask = {
    id: 'completed-video', name: '纪录片合集', kind: 'torrent' as const, status: 'completed' as const, progress: 100,
    completed_at: new Date(2026, 8, 10, 8).getTime() / 1000,
    files: [
      { index: 1, path: 'D:/Downloads/documentary.mp4', length: 1_073_741_824, completed_length: 1_073_741_824, selected: true },
      { index: 2, path: 'D:/Downloads/notes.txt', length: 524_288, completed_length: 524_288, selected: true },
    ],
  };
  render(<DownloadQueue
    tasks={[completedTask]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  expect(screen.getByText('2026-09-10 08:00')).toBeVisible();
  expect(screen.getByText('1.0 GB')).toBeVisible();
  expect(screen.getByRole('progressbar', { name: '纪录片合集 下载进度' })).toHaveAttribute('aria-valuenow', '100');
  expect(screen.getByText('下载完成')).toBeVisible();

  await user.click(screen.getByRole('button', { name: '打开 纪录片合集 所在目录' }));
  expect(onCommand).toHaveBeenCalledWith('completed-video', 'open_directory');
});

it.each([
  ['video', '播放视频', 'play_videos', ['episode.mp4', 'trailer.mkv']],
  ['audio', '播放音乐', 'play_videos', ['track.mp3', 'bonus.flac']],
  ['image', '查看图片', 'open_file', ['cover.jpg', 'back.png']],
] as const)('offers the %s action only when all selected files share that media type', async (type, label, command, names) => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const task = {
    id: `completed-${type}-collection`, name: `${type} collection`, kind: 'torrent' as const,
    status: 'completed' as const, progress: 100,
    files: [
      ...names.map((name, index) => ({ index: index + 1, path: `D:/Downloads/${name}`, length: 100, completed_length: 100, selected: true })),
      { index: 3, path: 'D:/Downloads/unselected.txt', length: 100, completed_length: 0, selected: false },
    ],
  };
  render(<DownloadQueue tasks={[task]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  await user.click(screen.getByRole('button', { name: `${label} ${task.name}` }));
  expect(onCommand).toHaveBeenCalledWith(task.id, command);
});

it('opens the directory when completed selected files mix video and audio', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const task = {
    id: 'completed-mixed-media', name: 'mixed media', kind: 'torrent' as const,
    status: 'completed' as const, progress: 100,
    files: [
      { index: 1, path: 'D:/Downloads/episode.mp4', length: 100, completed_length: 100, selected: true },
      { index: 2, path: 'D:/Downloads/soundtrack.mp3', length: 100, completed_length: 100, selected: true },
    ],
  };
  render(<DownloadQueue tasks={[task]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  await user.click(screen.getByRole('button', { name: '打开 mixed media 所在目录' }));
  expect(onCommand).toHaveBeenCalledWith(task.id, 'open_directory');
});

it.each([
  ['image', '查看图片', 'open_file', ['cover.jpg', 'nested/back.png']],
  ['video', '播放视频', 'play_videos', ['episode.mp4', 'nested/trailer.mkv']],
  ['audio', '播放音乐', 'play_videos', ['track.mp3', 'nested/bonus.flac']],
] as const)('classifies every descendant of a completed %s directory', async (type, label, command, names) => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const task = {
    id: `completed-${type}-directory`, name: `${type} directory`, kind: 'folder' as const,
    status: 'completed' as const, progress: 100,
    children: [
      { id: `${type}-first`, name: names[0], kind: 'cloud' as const, status: 'completed' as const, progress: 100 },
      { id: `${type}-nested`, name: 'nested', kind: 'folder' as const, status: 'completed' as const, progress: 100,
        children: [{ id: `${type}-second`, name: names[1].split('/').at(-1)!, kind: 'cloud' as const, status: 'completed' as const, progress: 100 }] },
    ],
  };
  render(<DownloadQueue tasks={[task]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  await user.click(screen.getByRole('button', { name: `${label} ${task.name}` }));
  expect(onCommand).toHaveBeenCalledWith(task.id, command);
});

it('opens a completed directory when descendant files have different types', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  const task = {
    id: 'completed-mixed-directory', name: 'mixed directory', kind: 'folder' as const,
    status: 'completed' as const, progress: 100,
    children: [
      { id: 'mixed-cover', name: 'cover.jpg', kind: 'cloud' as const, status: 'completed' as const, progress: 100 },
      { id: 'mixed-subdir', name: 'subdir', kind: 'folder' as const, status: 'completed' as const, progress: 100,
        children: [{ id: 'mixed-song', name: 'track.mp3', kind: 'cloud' as const, status: 'completed' as const, progress: 100 }] },
    ],
  };
  render(<DownloadQueue tasks={[task]} selectedTaskId={null} onSelect={() => {}} onCommand={onCommand} />);

  await user.click(screen.getByRole('button', { name: '打开 mixed directory 所在目录' }));
  expect(onCommand).toHaveBeenCalledWith(task.id, 'open_directory');
});

it('opens the download directory for a completed task without video files', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[{
      id: 'completed-archive', name: 'archive.zip', kind: 'file', status: 'completed', progress: 100,
      updated_at: new Date(2026, 8, 10, 8).getTime() / 1000,
      files: [{ index: 1, path: 'D:/Downloads/archive.zip', length: 1_572_864, completed_length: 1_572_864, selected: true }],
    }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  expect(screen.getByText('1.5 MB')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '打开 archive.zip 所在目录' }));
  expect(onCommand).toHaveBeenCalledWith('completed-archive', 'open_directory');
});

it('opens only completed images with the associated application and other files by directory', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[
      {
        id: 'completed-text', name: 'notes.txt', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/notes.txt', length: 128, completed_length: 128, selected: true }],
      },
      {
        id: 'completed-image', name: 'cover.png', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/cover.png', length: 256, completed_length: 256, selected: true }],
      },
      {
        id: 'completed-pdf', name: 'guide.pdf', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/guide.pdf', length: 512, completed_length: 512, selected: true }],
      },
      {
        id: 'completed-office', name: 'report.docx', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/report.docx', length: 1024, completed_length: 1024, selected: true }],
      },
    ]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  await user.click(screen.getByRole('button', { name: '打开 notes.txt 所在目录' }));
  expect(onCommand).toHaveBeenLastCalledWith('completed-text', 'open_directory');
  await user.click(screen.getByRole('button', { name: '查看图片 cover.png' }));
  expect(onCommand).toHaveBeenLastCalledWith('completed-image', 'open_file');
  await user.click(screen.getByRole('button', { name: '打开 guide.pdf 所在目录' }));
  expect(onCommand).toHaveBeenLastCalledWith('completed-pdf', 'open_directory');
  await user.click(screen.getByRole('button', { name: '打开 report.docx 所在目录' }));
  expect(onCommand).toHaveBeenLastCalledWith('completed-office', 'open_directory');
});

it('classifies completed local filenames without treating hash or question-mark characters as URL syntax', () => {
  render(<DownloadQueue
    tasks={[
      {
        id: 'completed-hash-text', name: 'notes#1.txt', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/notes#1.txt', length: 128, completed_length: 128, selected: true }],
      },
      {
        id: 'completed-disguised-executable', name: 'notes.txt#1.exe', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/notes.txt#1.exe', length: 128, completed_length: 128, selected: true }],
      },
      {
        id: 'completed-question-text', name: 'notes?1.txt', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/notes?1.txt', length: 128, completed_length: 128, selected: true }],
      },
      {
        id: 'completed-question-executable', name: 'notes.txt?1.exe', kind: 'file', status: 'completed', progress: 100,
        files: [{ index: 1, path: 'D:/Downloads/notes.txt?1.exe', length: 128, completed_length: 128, selected: true }],
      },
    ]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByRole('button', { name: '打开 notes#1.txt 所在目录' })).toBeVisible();
  expect(screen.getByRole('button', { name: '打开 notes.txt#1.exe 所在目录' })).toBeVisible();
  expect(screen.getByRole('button', { name: '打开 notes?1.txt 所在目录' })).toBeVisible();
  expect(screen.getByRole('button', { name: '打开 notes.txt?1.exe 所在目录' })).toBeVisible();
});

it('offers cleanup retry only from the typed completed-task state', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[
      {
        id: 'cleanup-pending', name: 'pending.bin', kind: 'cloud', status: 'completed', progress: 100,
        source: 'cloud-share://baidu/share/entry', cleanup_pending: true, error: 'opaque provider failure',
      },
      {
        id: 'cleanup-done', name: 'done.bin', kind: 'cloud', status: 'completed', progress: 100,
        source: 'cloud-share://baidu/share/other', cleanup_pending: false,
        error: 'cleanup_pending：这段旧文案不能决定界面能力',
      },
    ]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  await user.click(screen.getByRole('button', { name: '重试网盘清理 pending.bin' }));
  expect(onCommand).toHaveBeenLastCalledWith('cleanup-pending', 'retry_cleanup');
  expect(screen.queryByRole('button', { name: '重试网盘清理 done.bin' })).not.toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: '更多操作 pending.bin' }));
  await user.click(screen.getByRole('menuitem', { name: '重试网盘清理' }));
  expect(onCommand).toHaveBeenLastCalledWith('cleanup-pending', 'retry_cleanup');
});

it('keeps permanent deletion in the menu while the row exposes pause and more actions', async () => {
  const user = userEvent.setup();
  render(<DownloadQueue
    tasks={[{ id: 'active-actions', name: 'game.zip', kind: 'file', status: 'downloading', progress: 25 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  expect(screen.getByRole('button', { name: '暂停 game.zip' })).toBeVisible();
  expect(screen.getByRole('button', { name: '更多操作 game.zip' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '取消 game.zip' })).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '更多操作 game.zip' }));
  expect(screen.getByRole('menu', { name: 'game.zip任务菜单' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '删除任务' })).toBeVisible();
});

it('offers real state-aware commands when an active task is right-clicked', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[{ id: 'active-1', name: 'game.zip', source: 'https://example.com/game.zip', kind: 'file', status: 'downloading', progress: 25 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  fireEvent.contextMenu(screen.getByText('game.zip'), { clientX: 280, clientY: 180 });
  expect(screen.getByRole('menu', { name: 'game.zip任务菜单' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '暂停' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '删除任务' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '打开下载目录' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '重新下载' })).not.toBeInTheDocument();

  await user.click(screen.getByRole('menuitem', { name: '暂停' }));
  expect(onCommand).toHaveBeenCalledWith('active-1', 'pause');
  expect(screen.queryByRole('menu')).not.toBeInTheDocument();
});

it.each([
  ['下载中', 'downloading' as const],
  ['已完成', 'completed' as const],
  ['失败', 'failed' as const],
  ['回收站', 'recycled' as const],
])('shows the matching privacy move command for a normal %s task only when privacy downloads are enabled', (label, status) => {
  const { container, rerender } = render(<DownloadQueue
    tasks={[{ id: `${status}-normal`, name: `${label}-普通任务`, kind: 'file', status, progress: status === 'completed' || status === 'recycled' ? 100 : 0 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
    privacyEnabled={false}
  />);

  fireEvent.contextMenu(container.querySelector(`[data-task-id="${status}-normal"] .task-row`)!);
  expect(screen.queryByRole('menuitem', { name: '移入隐私下载' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '移入普通下载' })).not.toBeInTheDocument();

  rerender(<DownloadQueue
    tasks={[{ id: `${status}-normal`, name: `${label}-普通任务`, kind: 'file', status, progress: status === 'completed' || status === 'recycled' ? 100 : 0 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
    privacyEnabled
  />);
  fireEvent.contextMenu(container.querySelector(`[data-task-id="${status}-normal"] .task-row`)!);
  expect(screen.getByRole('menuitem', { name: '移入隐私下载' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '移入普通下载' })).not.toBeInTheDocument();
});

it.each([
  ['下载中', 'downloading' as const],
  ['已完成', 'completed' as const],
  ['失败', 'failed' as const],
  ['回收站', 'recycled' as const],
])('shows the matching privacy move command for a private %s task only when privacy downloads are enabled', (label, status) => {
  const task = { id: `${status}-private`, name: `${label}-隐私任务`, kind: 'file' as const, status, progress: status === 'completed' || status === 'recycled' ? 100 : 0, is_private: true };
  const { container, rerender } = render(<DownloadQueue tasks={[task]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} privacyEnabled={false} />);

  fireEvent.contextMenu(container.querySelector(`[data-task-id="${status}-private"] .task-row`)!);
  expect(screen.queryByRole('menuitem', { name: '移入隐私下载' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '移入普通下载' })).not.toBeInTheDocument();

  rerender(<DownloadQueue tasks={[task]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} privacyEnabled />);
  fireEvent.contextMenu(container.querySelector(`[data-task-id="${status}-private"] .task-row`)!);
  expect(screen.getByRole('menuitem', { name: '移入普通下载' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '移入隐私下载' })).not.toBeInTheDocument();
});

it('shows both privacy actions for a mixed multi-selection and forwards the requested target state', () => {
  const onBatchCommand = vi.fn();
  render(<DownloadQueue
    tasks={[
      { id: 'ordinary', name: '普通任务', kind: 'file', status: 'downloading', progress: 10 },
      { id: 'private', name: '隐私任务', kind: 'file', status: 'completed', progress: 100, is_private: true },
    ]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
    onBatchCommand={onBatchCommand}
    privacyEnabled
  />);

  fireEvent.click(screen.getByText('普通任务'));
  fireEvent.click(screen.getByText('隐私任务'), { ctrlKey: true });
  fireEvent.contextMenu(screen.getByText('普通任务'));
  expect(screen.getByRole('menuitem', { name: '移入隐私下载' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '移入普通下载' })).toBeVisible();

  fireEvent.click(screen.getByRole('menuitem', { name: '移入隐私下载' }));
  expect(onBatchCommand).toHaveBeenCalledWith(['ordinary', 'private'], 'move_to_private');
});

it('offers permanent deletion for an invalid link left unsubmitted in parsing state', async () => {
  const user = userEvent.setup();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[{ id: 'invalid-link', name: '非法下载链接', source: 'not-a-download-link', kind: 'file', status: 'parsing', progress: 0 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={onCommand}
  />);

  fireEvent.contextMenu(screen.getByText('非法下载链接'));
  await user.click(screen.getByRole('menuitem', { name: '删除任务' }));

  expect(onCommand).toHaveBeenCalledWith('invalid-link', 'delete');
});

it('keeps modifier-selected tasks together and exposes only batch-safe menu actions', async () => {
  const onSelect = vi.fn();
  const onBatchCommand = vi.fn();
  render(<DownloadQueue
    tasks={[
      { id: 'running-1', name: 'first.zip', source: 'https://example.com/first.zip', kind: 'file', status: 'downloading', progress: 25 },
      { id: 'paused-2', name: 'second.zip', source: 'https://example.com/second.zip', kind: 'file', status: 'paused', progress: 50 },
    ]}
    selectedTaskId={null}
    onSelect={onSelect}
    onCommand={() => {}}
    onBatchCommand={onBatchCommand}
  />);

  fireEvent.click(screen.getByText('first.zip'));
  fireEvent.click(screen.getByText('second.zip'), { ctrlKey: true });

  const firstRow = screen.getByText('first.zip').closest('.task-row');
  const secondRow = screen.getByText('second.zip').closest('.task-row');
  expect(firstRow).toHaveAttribute('aria-selected', 'true');
  expect(secondRow).toHaveAttribute('aria-selected', 'true');

  fireEvent.contextMenu(screen.getByText('first.zip'), { clientX: 280, clientY: 180 });
  expect(screen.getByRole('menu', { name: '已选择 2 个任务任务菜单' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '开始下载' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '暂停' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '删除' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '复制下载链接' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '打开下载目录' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '查看任务详情' })).not.toBeInTheDocument();

  fireEvent.click(screen.getByRole('menuitem', { name: '复制下载链接' }));
  expect(onBatchCommand).toHaveBeenCalledWith(['running-1', 'paused-2'], 'copy_link');
});

it('reports the full Ctrl-click selection to its task-list parent', () => {
  const onSelectionChange = vi.fn();
  render(<DownloadQueue
    tasks={[
      { id: 'select-1', name: 'first.zip', kind: 'file', status: 'queued', progress: 0 },
      { id: 'select-2', name: 'second.zip', kind: 'file', status: 'queued', progress: 0 },
    ]}
    privacyEnabled={false}
    selectedTaskId={null}
    onSelect={() => {}}
    onSelectionChange={onSelectionChange}
    onCommand={() => {}}
  />);

  fireEvent.click(screen.getByText('first.zip'));
  fireEvent.click(screen.getByText('second.zip'), { ctrlKey: true });

  expect(onSelectionChange).toHaveBeenLastCalledWith(['select-1', 'select-2']);
});

it('keeps the original selection anchor when Shift follows a Ctrl multi-selection', () => {
  function ControlledQueue() {
    const [selectedTaskIds, setSelectedTaskIds] = useState<string[]>([]);
    return <DownloadQueue
      tasks={[
        { id: 'controlled-1', name: 'first.zip', kind: 'file', status: 'queued', progress: 0 },
        { id: 'controlled-2', name: 'second.zip', kind: 'file', status: 'queued', progress: 0 },
        { id: 'controlled-3', name: 'third.zip', kind: 'file', status: 'queued', progress: 0 },
      ]}
      privacyEnabled={false}
      selectedTaskId={selectedTaskIds.at(-1) ?? null}
      selectedTaskIds={selectedTaskIds}
      onSelect={() => {}}
      onSelectionChange={(ids) => setSelectedTaskIds([...ids])}
      onCommand={() => {}}
      onReorder={() => {}}
    />;
  }

  render(<ControlledQueue />);

  fireEvent.click(screen.getByText('first.zip'));
  fireEvent.click(screen.getByText('second.zip'), { ctrlKey: true });
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(2);

  fireEvent.click(screen.getByText('third.zip'), { shiftKey: true });
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(3);
});

it('honors document-level modifier keys when a WebView click omits them', () => {
  function ControlledQueue() {
    const [selectedTaskIds, setSelectedTaskIds] = useState<string[]>([]);
    return <DownloadQueue
      tasks={[
        { id: 'native-modifier-1', name: 'first.zip', kind: 'file', status: 'queued', progress: 0 },
        { id: 'native-modifier-2', name: 'second.zip', kind: 'file', status: 'queued', progress: 0 },
        { id: 'native-modifier-3', name: 'third.zip', kind: 'file', status: 'queued', progress: 0 },
      ]}
      privacyEnabled={false}
      selectedTaskId={selectedTaskIds.at(-1) ?? null}
      selectedTaskIds={selectedTaskIds}
      onSelect={() => {}}
      onSelectionChange={(ids) => setSelectedTaskIds([...ids])}
      onCommand={() => {}}
      onReorder={() => {}}
    />;
  }

  render(<ControlledQueue />);
  fireEvent.click(screen.getByText('first.zip'));

  fireEvent.keyDown(document, { key: 'Control' });
  fireEvent.click(screen.getByText('second.zip'));
  fireEvent.keyUp(document, { key: 'Control' });
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(2);

  fireEvent.keyDown(document, { key: 'Shift' });
  fireEvent.click(screen.getByText('third.zip'));
  fireEvent.keyUp(document, { key: 'Shift' });
  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(3);
});

it('does not turn a Ctrl-click into a task drag', () => {
  class TestPointerEvent extends MouseEvent {
    pointerId: number;
    constructor(type: string, init: MouseEventInit & { pointerId?: number } = {}) {
      super(type, init);
      this.pointerId = init.pointerId ?? 0;
    }
  }
  const originalPointerEvent = window.PointerEvent;
  Object.defineProperty(window, 'PointerEvent', { configurable: true, value: TestPointerEvent });
  function ControlledQueue() {
    const [selectedTaskIds, setSelectedTaskIds] = useState<string[]>([]);
    return <DownloadQueue
      tasks={[
        { id: 'drag-select-1', name: 'first.zip', kind: 'file', status: 'queued', progress: 0 },
        { id: 'drag-select-2', name: 'second.zip', kind: 'file', status: 'queued', progress: 0 },
      ]}
      privacyEnabled={false}
      selectedTaskId={selectedTaskIds.at(-1) ?? null}
      selectedTaskIds={selectedTaskIds}
      onSelect={() => {}}
      onSelectionChange={(ids) => setSelectedTaskIds([...ids])}
      onCommand={() => {}}
      onReorder={() => {}}
    />;
  }

  render(<ControlledQueue />);
  const first = screen.getByText('first.zip').closest('.task-row')!;
  const second = screen.getByText('second.zip').closest('.task-row')!;
  const secondGroup = second.closest('.task-group')!;
  vi.spyOn(secondGroup, 'getBoundingClientRect').mockReturnValue({ top: 0, bottom: 40, height: 40, left: 0, right: 100, width: 100, x: 0, y: 0, toJSON: () => ({}) });

  fireEvent.click(first);
  fireEvent.pointerDown(second, { pointerId: 18, button: 0, clientX: 5, clientY: 5, ctrlKey: true });
  fireEvent.pointerMove(second, { pointerId: 18, clientX: 5, clientY: 18, ctrlKey: true });
  fireEvent.pointerUp(second, { pointerId: 18, clientX: 5, clientY: 18, ctrlKey: true });
  fireEvent.click(second, { ctrlKey: true });

  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(2);
  expect(document.querySelector('.task-drag-preview')).not.toBeInTheDocument();
  if (originalPointerEvent) Object.defineProperty(window, 'PointerEvent', { configurable: true, value: originalPointerEvent });
  else delete (window as Window & { PointerEvent?: typeof window.PointerEvent }).PointerEvent;
});

it('selects a contiguous task range with Shift-click', () => {
  render(<DownloadQueue
    tasks={[
      { id: 'range-1', name: 'one.zip', kind: 'file', status: 'queued', progress: 0 },
      { id: 'range-2', name: 'two.zip', kind: 'file', status: 'queued', progress: 0 },
      { id: 'range-3', name: 'three.zip', kind: 'file', status: 'queued', progress: 0 },
    ]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  fireEvent.click(screen.getByText('one.zip'));
  fireEvent.click(screen.getByText('three.zip'), { shiftKey: true });

  expect(document.querySelectorAll('.task-row[aria-selected="true"]')).toHaveLength(3);
});

it('offers terminal-task deletion without showing invalid transfer commands', () => {
  render(<DownloadQueue
    tasks={[{ id: 'done-1', name: 'finished.iso', source: 'https://example.com/finished.iso', kind: 'file', status: 'completed', progress: 100 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  fireEvent.contextMenu(screen.getByText('finished.iso'));
  expect(screen.getByRole('menuitem', { name: '打开下载目录' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '复制下载链接' })).toBeVisible();
  expect(screen.getByRole('menuitem', { name: '删除任务' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '暂停' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '取消任务' })).not.toBeInTheDocument();
});

it('omits terminal status text from the more-menu header', () => {
  render(<DownloadQueue
    tasks={[{ id: 'done-menu-1', name: 'wallpaper 4.jpg', kind: 'file', status: 'completed', progress: 100 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  fireEvent.contextMenu(screen.getByText('wallpaper 4.jpg'));

  const menu = screen.getByRole('menu', { name: 'wallpaper 4.jpg任务菜单' });
  expect(menu.querySelector('header')).toBeNull();
  expect(menu).not.toHaveTextContent('下载完成');
});

it('dismisses the task menu with Escape', () => {
  render(<DownloadQueue
    tasks={[folderTask]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  fireEvent.contextMenu(screen.getByText('Season 1'));
  expect(screen.getByRole('menu')).toBeVisible();
  fireEvent.keyDown(document, { key: 'Escape' });
  expect(screen.queryByRole('menu')).not.toBeInTheDocument();
});

it('uses the same deletion action for a legacy recycled task', () => {
  render(<DownloadQueue
    tasks={[{ id: 'recycled-1', name: 'old.iso', kind: 'file', status: 'recycled', progress: 100 }]}
    selectedTaskId={null}
    onSelect={() => {}}
    onCommand={() => {}}
  />);

  fireEvent.contextMenu(screen.getByText('old.iso'));
  expect(screen.getByRole('menuitem', { name: '删除任务' })).toBeVisible();
  expect(screen.queryByRole('menuitem', { name: '移入回收站' })).not.toBeInTheDocument();
  expect(screen.queryByRole('menuitem', { name: '重新下载' })).not.toBeInTheDocument();
});

it('selects a task on ordinary click without emitting a task-details command', async () => {
  const user = userEvent.setup();
  const onSelect = vi.fn();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[{ id: 'task-1', name: 'archive.zip', kind: 'file', status: 'queued', progress: 0 }]}
    selectedTaskId={null}
    onSelect={onSelect}
    onCommand={onCommand}
  />);

  await user.click(screen.getByText('archive.zip'));
  expect(onSelect).toHaveBeenCalledWith('task-1');
  expect(onCommand).not.toHaveBeenCalled();
});

it('emits details only after its right-click menu item is selected', async () => {
  const user = userEvent.setup();
  const onSelect = vi.fn();
  const onCommand = vi.fn();
  render(<DownloadQueue
    tasks={[{ id: 'task-1', name: 'archive.zip', kind: 'file', status: 'queued', progress: 0 }]}
    selectedTaskId={null}
    onSelect={onSelect}
    onCommand={onCommand}
  />);

  fireEvent.contextMenu(screen.getByText('archive.zip'));
  expect(onSelect).toHaveBeenCalledWith('task-1');
  expect(onCommand).not.toHaveBeenCalled();
  await user.click(screen.getByRole('menuitem', { name: '查看任务详情' }));
  expect(onCommand).toHaveBeenCalledTimes(1);
  expect(onCommand).toHaveBeenCalledWith('task-1', 'details');
});

it.each([
  ['download.xunlei.native', '迅雷原生通道'],
  ['download.xunlei.accelerating', '迅雷会员通道'],
])('localizes the Mac native channel status %s', (statusMessage, label) => {
  render(<DownloadQueue tasks={[{ id: 'native', name: 'file.bin', source: 'cloud://xunlei/file', kind: 'cloud', status: 'downloading', progress: 20, statusMessage }]}
    selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
  expect(screen.getByText(label)).toBeVisible();
  expect(screen.queryByText(statusMessage)).not.toBeInTheDocument();
});
