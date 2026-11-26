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
    { id: 'sharing', name: 'sharing.mkv', kind: 'magnet', status: 'completed', progress: 100, completed_at: 1_789_095_720, p2p_sharing: true },
    { id: 'finished', name: 'finished.zip', kind: 'ed2k', status: 'completed', progress: 100, completed_at: 1_789_095_720, p2p_sharing: false },
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
        completed_at: 1_789_095_720, total_length: 1_048_576, completed_length: 1_048_576,
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
          completed_at: 1_789_095_720,
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

it('marks a public MEGA link download that uses the account quota', () => {
  render(<DownloadQueue tasks={[{ id: 'mega', name: 'file.bin', source: 'cloud-share://mega/file/AbC12345/AbC12345', kind: 'cloud', status: 'downloading', progress: 20, statusMessage: 'download.mega.accountQuota' }]}
    selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
  expect(screen.getByText('使用 MEGA 账号配额')).toBeVisible();
  expect(screen.queryByText('download.mega.accountQuota')).not.toBeInTheDocument();
});

it('keeps the Mac native channel fallback reason available after a quick download finishes', () => {
  const task = { id: 'file', name: 'file.bin', kind: 'cloud' as const, statusMessage: 'download.xunlei.unsupportedFallback:5.80.7 (66660)' };
  const props = { selectedTaskId: null, onSelect() {}, onCommand() {} };
  const reason = '本机迅雷版本不受支持，已改用普通下载。支持的版本为 5.80.7 (66660)。';
  const { rerender } = render(<DownloadQueue {...props} tasks={[{ ...task, status: 'downloading', progress: 5 }]} />);
  expect(screen.getByText(reason)).toBeVisible();
  rerender(<DownloadQueue {...props} tasks={[{ ...task, status: 'completed', progress: 100 }]} />);
  expect(screen.getByText('已完成 · 普通下载')).toHaveAttribute('title', reason);
});

it('shows the Mac native channel failure for both the directory and its expanded file', async () => {
  const error = 'download.xunlei.proxyCheckpoint';
  const message = '此迅雷断点需要直连。关闭代理后可继续，或新建任务通过代理下载。';
  render(<DownloadQueue tasks={[{
    id: 'dir', name: 'files', kind: 'folder', status: 'failed', progress: 5, error,
    children: [{ id: 'leaf', name: 'video.mp4', kind: 'cloud', status: 'failed', progress: 5, error }],
  }]} privacyEnabled={false} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
  expect(screen.getByText(message)).toBeVisible();
  await userEvent.click(screen.getByRole('button', { name: '展开 files' }));
  expect(screen.getAllByText(message)).toHaveLength(2);
});

it('shows a Mac native channel fallback on the affected file in an expanded directory', async () => {
  const statusMessage = 'download.xunlei.unsupportedFallback:5.80.7 (66660)';
  render(<DownloadQueue tasks={[{
    id: 'dir', name: 'files', kind: 'folder', status: 'downloading', progress: 5,
    children: [{ id: 'leaf', name: 'video.mp4', kind: 'cloud', status: 'downloading', progress: 5, statusMessage }],
  }]} selectedTaskId={null} onSelect={() => {}} onCommand={() => {}} />);
  await userEvent.click(screen.getByRole('button', { name: '展开 files' }));
  expect(screen.getByText(/已改用普通下载/)).toBeVisible();
});
